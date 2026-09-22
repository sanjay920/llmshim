//! Horizontal-scale rate limiting and backpressure for the proxy.
//!
//! This is the *proactive* layer that sits in front of the reactive retry logic
//! in [`crate::client`]. Where the client honors `Retry-After` *after* an
//! upstream 429, this layer sheds load *before* dispatching so a fleet of proxy
//! replicas doesn't collectively blow provider TPM/RPM limits.
//!
//! Three cooperating pieces:
//!
//! 1. [`RateLimiter`] — a pluggable token-bucket coordinator. The default
//!    [`InMemoryRateLimiter`] governs a single instance with zero infra. The
//!    opt-in [`RedisRateLimiter`] (feature `redis-coordination`) shares one
//!    bucket across replicas for a true global limit.
//! 2. [`Backpressure`] — a per-instance concurrency cap (a semaphore) with a
//!    bounded wait, so at 10k concurrency we return 503 instead of growing
//!    memory without bound.
//! 3. [`RateLimitConfig`] — 12-factor env configuration, all optional with safe
//!    defaults (no limits ⇒ the limiter is a no-op).
//!
//! The token-bucket math is factored into a small pure [`TokenBucket`] type so
//! it can be unit-tested deterministically with `tokio::time` paused.

#[cfg(any(feature = "gateway", test))]
use super::convert::{active_native_namespace, AdmissionTarget, PreparedRequest};
use super::types::ChatRequest;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// Providers we recognize for per-provider env overrides.
const KNOWN_PROVIDERS: &[&str] = &["openai", "anthropic", "gemini", "xai"];

/// Default per-instance in-flight upstream request cap.
const DEFAULT_MAX_CONCURRENCY: usize = 256;
/// Default bound on how long a request waits for a concurrency permit.
const DEFAULT_QUEUE_TIMEOUT_MS: u64 = 5_000;
/// Fallback penalty applied to a bucket after an upstream 429 with no better hint.
const DEFAULT_PENALTY_SECS: u64 = 5;
/// Assumed output size when a request specifies no `max_tokens`, for TPM estimation.
const DEFAULT_MAX_TOKENS_ESTIMATE: u64 = 1024;
/// Retry hint for an explicit zero limit, which is a permanent policy denial.
const ZERO_LIMIT_RETRY_AFTER: Duration = Duration::from_secs(60);

// ===========================================================================
// RateKey
// ===========================================================================

/// Identifies a rate-limit bucket. Today the proxy holds one API key per
/// provider, so keying by provider is sufficient — but the optional `tenant`
/// field keeps the type extensible (e.g. per-caller / per-api-key-hash limits)
/// without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateKey {
    pub provider: String,
    pub tenant: Option<String>,
}

impl RateKey {
    /// Bucket keyed by provider only (the common case).
    pub fn provider(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            tenant: None,
        }
    }

    /// Stable string form used as the Redis key suffix.
    #[cfg(feature = "redis-coordination")]
    fn as_str(&self) -> String {
        match &self.tenant {
            Some(t) => format!("{}:{}", self.provider, t),
            None => self.provider.clone(),
        }
    }
}

/// Suggested wait returned when a bucket is exhausted. Never blocks the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub Duration);

// ===========================================================================
// RateLimiter trait
// ===========================================================================

/// A pluggable rate-limit coordinator.
///
/// Implementations must be cheap to share (`Arc<dyn RateLimiter>`) and safe to
/// call from many tasks concurrently.
#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync {
    /// Try to admit a request costing `permits` tokens (an estimate of the
    /// request's token footprint, used for the optional TPM bucket; the RPM
    /// bucket always costs one request).
    ///
    /// Returns `Ok(())` if admitted. On exhaustion returns `Err(RetryAfter)`
    /// with the suggested wait — it never blocks the caller indefinitely.
    async fn acquire(&self, key: &RateKey, permits: u32) -> Result<(), RetryAfter>;

    /// Record an upstream 429 for `key` so its bucket backs off. When the
    /// coordinator is distributed this backoff is shared across replicas.
    async fn penalize(&self, key: &RateKey, retry_after: Duration);
}

// ===========================================================================
// TokenBucket — pure, deterministic math
// ===========================================================================

/// A single continuously-refilling token bucket.
///
/// Time is threaded in explicitly (`tokio::time::Instant`) so refill is a pure
/// function of elapsed time — deterministically testable with `tokio::time`
/// paused/advanced, and correct without wall-clock sleeps.
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Maximum tokens (also the burst size).
    capacity: f64,
    /// Tokens replenished per second.
    refill_per_sec: f64,
    /// Current token balance.
    tokens: f64,
    /// Timestamp of the last refill.
    last: Instant,
    /// If set and in the future, the bucket is penalized until this instant.
    penalty_until: Option<Instant>,
}

impl TokenBucket {
    /// A bucket sized from a per-minute rate (RPM/TPM): capacity == `per_minute`
    /// (burst), refilling at `per_minute / 60` per second. Starts full.
    fn from_per_minute(per_minute: f64, now: Instant) -> Self {
        let capacity = if per_minute == 0.0 {
            0.0
        } else {
            per_minute.max(1.0)
        };
        Self {
            capacity,
            refill_per_sec: if per_minute == 0.0 {
                0.0
            } else {
                (per_minute / 60.0).max(f64::MIN_POSITIVE)
            },
            tokens: capacity,
            last: now,
            penalty_until: None,
        }
    }

    /// Replenish tokens for the time elapsed since the last refill.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last = now;
        }
    }

    /// Whether `permits` could be taken right now. Assumes `refill` was just
    /// called. Returns the suggested wait on failure (penalty window, else the
    /// time for the deficit to refill).
    fn check(&self, permits: f64, now: Instant) -> Result<(), Duration> {
        if let Some(until) = self.penalty_until {
            if now < until {
                return Err(until.saturating_duration_since(now));
            }
        }
        // Small epsilon guards against float rounding leaving us a hair short.
        if self.tokens + 1e-9 >= permits {
            Ok(())
        } else {
            if self.refill_per_sec <= 0.0 {
                return Err(ZERO_LIMIT_RETRY_AFTER);
            }
            let deficit = permits - self.tokens;
            Err(Duration::from_secs_f64(deficit / self.refill_per_sec))
        }
    }

    /// Deduct `permits`. Only call after a successful [`check`].
    fn commit(&mut self, permits: f64) {
        self.tokens -= permits;
    }

    /// Back off: extend the penalty window and drain tokens so we stop admitting
    /// immediately. Takes the later of any existing penalty and the new one.
    fn penalize(&mut self, retry_after: Duration, now: Instant) {
        let until = now + retry_after;
        self.penalty_until = Some(match self.penalty_until {
            Some(existing) if existing > until => existing,
            _ => until,
        });
        self.tokens = 0.0;
    }
}

/// The RPM and/or TPM buckets governing one [`RateKey`].
#[derive(Debug, Clone)]
struct KeyBuckets {
    rpm: Option<TokenBucket>,
    tpm: Option<TokenBucket>,
}

impl KeyBuckets {
    /// Atomically admit one request (1 RPM token, `tpm_permits` TPM tokens).
    /// Both buckets must have room or neither is charged — returns the larger
    /// suggested wait on failure.
    fn acquire(&mut self, tpm_permits: f64, now: Instant) -> Result<(), Duration> {
        if let Some(b) = self.rpm.as_mut() {
            b.refill(now);
        }
        if let Some(b) = self.tpm.as_mut() {
            b.refill(now);
        }

        let mut wait: Option<Duration> = None;
        let mut note = |w: Duration| wait = Some(wait.map_or(w, |cur| cur.max(w)));

        if let Some(b) = self.rpm.as_ref() {
            if let Err(w) = b.check(1.0, now) {
                note(w);
            }
        }
        if let Some(b) = self.tpm.as_ref() {
            if let Err(w) = b.check(tpm_permits, now) {
                note(w);
            }
        }

        if let Some(w) = wait {
            return Err(w);
        }

        if let Some(b) = self.rpm.as_mut() {
            b.commit(1.0);
        }
        if let Some(b) = self.tpm.as_mut() {
            b.commit(tpm_permits);
        }
        Ok(())
    }

    fn penalize(&mut self, retry_after: Duration, now: Instant) {
        if let Some(b) = self.rpm.as_mut() {
            b.penalize(retry_after, now);
        }
        if let Some(b) = self.tpm.as_mut() {
            b.penalize(retry_after, now);
        }
    }
}

// ===========================================================================
// Config
// ===========================================================================

/// Resolved RPM/TPM limits for one provider. `None` fields mean "no limit".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProviderLimit {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
}

impl ProviderLimit {
    fn is_unlimited(&self) -> bool {
        self.rpm.is_none() && self.tpm.is_none()
    }

    fn has_zero_limit(&self) -> bool {
        self.rpm == Some(0) || self.tpm == Some(0)
    }
}

/// Rate-limit configuration: global defaults plus optional per-provider overrides.
#[derive(Debug, Clone, Default)]
pub struct RateLimitConfig {
    global: ProviderLimit,
    per_provider: HashMap<String, ProviderLimit>,
}

/// How long to back a bucket off after an upstream 429. The reactive layer in
/// [`crate::client`] already consumed the provider's `Retry-After`, so this
/// proactive layer applies a fixed, tunable penalty. Override with
/// `LLMSHIM_PENALTY_SECS` (default 5s).
pub fn penalty_duration() -> Duration {
    Duration::from_secs(env_u64("LLMSHIM_PENALTY_SECS").unwrap_or(DEFAULT_PENALTY_SECS))
}

impl RateLimitConfig {
    /// Read limits from the environment (all optional):
    /// - `LLMSHIM_RATE_LIMIT_RPM` / `LLMSHIM_RATE_LIMIT_TPM` — global defaults.
    /// - `LLMSHIM_<PROVIDER>_RPM` / `_TPM` — per-provider overrides
    ///   (`LLMSHIM_OPENAI_RPM`, `LLMSHIM_ANTHROPIC_TPM`, …).
    pub fn from_env() -> Self {
        let global = ProviderLimit {
            rpm: env_u32("LLMSHIM_RATE_LIMIT_RPM"),
            tpm: env_u32("LLMSHIM_RATE_LIMIT_TPM"),
        };

        let mut per_provider = HashMap::new();
        for p in KNOWN_PROVIDERS {
            let up = p.to_uppercase();
            let limit = ProviderLimit {
                rpm: env_u32(&format!("LLMSHIM_{up}_RPM")),
                tpm: env_u32(&format!("LLMSHIM_{up}_TPM")),
            };
            if !limit.is_unlimited() {
                per_provider.insert(p.to_string(), limit);
            }
        }

        Self {
            global,
            per_provider,
        }
    }

    /// Config with only global RPM/TPM limits and no per-provider overrides.
    pub fn with_global(rpm: Option<u32>, tpm: Option<u32>) -> Self {
        Self {
            global: ProviderLimit { rpm, tpm },
            per_provider: HashMap::new(),
        }
    }

    /// Effective limit for a provider: per-provider override falls back to global
    /// per field (e.g. a provider RPM override still inherits the global TPM).
    pub fn resolve(&self, provider: &str) -> ProviderLimit {
        let ov = self.per_provider.get(provider).copied().unwrap_or_default();
        ProviderLimit {
            rpm: ov.rpm.or(self.global.rpm),
            tpm: ov.tpm.or(self.global.tpm),
        }
    }

    /// True when no limits at all are configured — the limiter is a no-op.
    pub fn is_unlimited(&self) -> bool {
        self.global.is_unlimited() && self.per_provider.is_empty()
    }
}

// ===========================================================================
// InMemoryRateLimiter (default)
// ===========================================================================

/// Per-instance token-bucket limiter. Zero infrastructure; governs a single
/// replica. A `Mutex<HashMap>` is fine here: the critical section is a few
/// float ops, dwarfed by the upstream network call it guards.
pub struct InMemoryRateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<RateKey, KeyBuckets>>,
}

impl InMemoryRateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn buckets_for(&self, limit: ProviderLimit, now: Instant) -> KeyBuckets {
        KeyBuckets {
            rpm: limit
                .rpm
                .map(|r| TokenBucket::from_per_minute(r as f64, now)),
            tpm: limit
                .tpm
                .map(|t| TokenBucket::from_per_minute(t as f64, now)),
        }
    }
}

#[async_trait::async_trait]
impl RateLimiter for InMemoryRateLimiter {
    async fn acquire(&self, key: &RateKey, permits: u32) -> Result<(), RetryAfter> {
        let limit = self.config.resolve(&key.provider);
        if limit.is_unlimited() {
            return Ok(()); // no-op when unconfigured
        }
        if limit.has_zero_limit() {
            return Err(RetryAfter(ZERO_LIMIT_RETRY_AFTER));
        }
        let now = Instant::now();
        let mut map = self.buckets.lock().await;
        let entry = map
            .entry(key.clone())
            .or_insert_with(|| self.buckets_for(limit, now));
        entry
            .acquire(permits.max(1) as f64, now)
            .map_err(RetryAfter)
    }

    async fn penalize(&self, key: &RateKey, retry_after: Duration) {
        let limit = self.config.resolve(&key.provider);
        if limit.is_unlimited() || limit.has_zero_limit() {
            return;
        }
        let now = Instant::now();
        let mut map = self.buckets.lock().await;
        let entry = map
            .entry(key.clone())
            .or_insert_with(|| self.buckets_for(limit, now));
        entry.penalize(retry_after, now);
    }
}

// ===========================================================================
// RedisRateLimiter (opt-in, feature = "redis-coordination")
// ===========================================================================

#[cfg(feature = "redis-coordination")]
mod redis_impl {
    use super::*;
    use redis::aio::ConnectionManager;
    use tokio::sync::OnceCell;

    /// Atomically checks the configured RPM and TPM buckets in one Redis Lua
    /// invocation. Each bucket stores `tokens`, last refill `ts` (ms), and a
    /// `penalty` deadline (ms). Returns `{request_admitted, retry_after_ms}`.
    const RATE_LIMIT_LUA: &str = r#"
        local current_timestamp_ms = tonumber(ARGV[1])
        local key_ttl_ms = tonumber(ARGV[2])

        local rpm_enabled = tonumber(ARGV[3]) == 1
        local rpm_capacity = tonumber(ARGV[4])
        local rpm_refill_per_second = tonumber(ARGV[5])
        local rpm_required_tokens = tonumber(ARGV[6])

        local tpm_enabled = tonumber(ARGV[7]) == 1
        local tpm_capacity = tonumber(ARGV[8])
        local tpm_refill_per_second = tonumber(ARGV[9])
        local tpm_required_tokens = tonumber(ARGV[10])

        local function load_bucket(bucket_key, capacity, refill_per_second)
            local stored_bucket_values = redis.call('HMGET', bucket_key, 'tokens', 'ts', 'penalty')
            local token_balance = tonumber(stored_bucket_values[1])
            local last_refill_timestamp_ms = tonumber(stored_bucket_values[2])
            local penalty_deadline_ms = tonumber(stored_bucket_values[3]) or 0
            if token_balance == nil then token_balance = capacity end
            if last_refill_timestamp_ms == nil then last_refill_timestamp_ms = current_timestamp_ms end

            local elapsed_ms = current_timestamp_ms - last_refill_timestamp_ms
            if elapsed_ms < 0 then elapsed_ms = 0 end
            token_balance = math.min(
                capacity,
                token_balance + (elapsed_ms / 1000.0) * refill_per_second
            )
            return token_balance, current_timestamp_ms, penalty_deadline_ms
        end

        local function retry_after_ms(token_balance, penalty_deadline_ms, refill_per_second, required_tokens)
            if penalty_deadline_ms > current_timestamp_ms then
                return penalty_deadline_ms - current_timestamp_ms
            end
            if token_balance >= required_tokens then
                return nil
            end
            return math.ceil((required_tokens - token_balance) / refill_per_second * 1000.0)
        end

        local rpm_token_balance, rpm_last_refill_timestamp_ms, rpm_penalty_deadline_ms
        if rpm_enabled then
            rpm_token_balance, rpm_last_refill_timestamp_ms, rpm_penalty_deadline_ms = load_bucket(
                KEYS[1], rpm_capacity, rpm_refill_per_second
            )
        end

        local tpm_token_balance, tpm_last_refill_timestamp_ms, tpm_penalty_deadline_ms
        if tpm_enabled then
            tpm_token_balance, tpm_last_refill_timestamp_ms, tpm_penalty_deadline_ms = load_bucket(
                KEYS[2], tpm_capacity, tpm_refill_per_second
            )
        end

        local request_admitted = 1
        local maximum_retry_after_ms = 0
        if rpm_enabled then
            local rpm_retry_after_ms = retry_after_ms(
                rpm_token_balance,
                rpm_penalty_deadline_ms,
                rpm_refill_per_second,
                rpm_required_tokens
            )
            if rpm_retry_after_ms ~= nil then
                request_admitted = 0
                maximum_retry_after_ms = math.max(maximum_retry_after_ms, rpm_retry_after_ms)
            end
        end
        if tpm_enabled then
            local tpm_retry_after_ms = retry_after_ms(
                tpm_token_balance,
                tpm_penalty_deadline_ms,
                tpm_refill_per_second,
                tpm_required_tokens
            )
            if tpm_retry_after_ms ~= nil then
                request_admitted = 0
                maximum_retry_after_ms = math.max(maximum_retry_after_ms, tpm_retry_after_ms)
            end
        end

        if request_admitted == 1 then
            if rpm_enabled then rpm_token_balance = rpm_token_balance - rpm_required_tokens end
            if tpm_enabled then tpm_token_balance = tpm_token_balance - tpm_required_tokens end
        end

        if rpm_enabled then
            redis.call(
                'HSET',
                KEYS[1],
                'tokens', rpm_token_balance,
                'ts', rpm_last_refill_timestamp_ms,
                'penalty', rpm_penalty_deadline_ms
            )
            redis.call('PEXPIRE', KEYS[1], key_ttl_ms)
        end
        if tpm_enabled then
            redis.call(
                'HSET',
                KEYS[2],
                'tokens', tpm_token_balance,
                'ts', tpm_last_refill_timestamp_ms,
                'penalty', tpm_penalty_deadline_ms
            )
            redis.call('PEXPIRE', KEYS[2], key_ttl_ms)
        end
        return {request_admitted, maximum_retry_after_ms}
    "#;

    const PENALTY_LUA: &str = r#"
        local penalty_deadline_ms = tonumber(ARGV[1])
        local key_ttl_ms = tonumber(ARGV[2])
        redis.call('HSET', KEYS[1], 'penalty', penalty_deadline_ms, 'tokens', 0)
        redis.call('PEXPIRE', KEYS[1], key_ttl_ms)
        return 1
    "#;

    /// Keys expire after an hour of inactivity — long enough to preserve state
    /// across bursts, short enough not to leak buckets for retired tenants.
    const KEY_TTL_MS: u64 = 3_600_000;

    /// Distributed token-bucket limiter backed by Redis. A single shared bucket
    /// per key across all replicas gives a true global limit.
    ///
    /// Fails **open**: if Redis is unreachable the limiter admits the request
    /// (and logs) rather than taking the proxy down — the reactive layer in
    /// [`crate::client`] still protects against provider 429s.
    pub struct RedisRateLimiter {
        client: redis::Client,
        connection_manager: OnceCell<ConnectionManager>,
        config: RateLimitConfig,
        rate_limit_script: redis::Script,
        penalty_script: redis::Script,
    }

    impl RedisRateLimiter {
        /// Parse the URL and build the client. The async connection is
        /// established lazily on first use, so this stays synchronous and can be
        /// called from the non-async `app()` builder.
        pub fn new(url: &str, config: RateLimitConfig) -> redis::RedisResult<Self> {
            Ok(Self {
                client: redis::Client::open(url)?,
                connection_manager: OnceCell::new(),
                config,
                rate_limit_script: redis::Script::new(RATE_LIMIT_LUA),
                penalty_script: redis::Script::new(PENALTY_LUA),
            })
        }

        async fn connection(&self) -> redis::RedisResult<ConnectionManager> {
            self.connection_manager
                .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
                .await
                .cloned()
        }

        fn redis_key(bucket_dimension: &str, rate_key: &RateKey) -> String {
            format!("llmshim:rl:{}:{}", rate_key.as_str(), bucket_dimension)
        }

        /// Atomically check and debit the configured RPM and TPM dimensions.
        /// `Ok(None)` = admitted, `Ok(Some(wait))` = denied, `Err` = Redis
        /// failure (the caller preserves the documented fail-open behavior).
        async fn check_and_debit(
            &self,
            connection: &mut ConnectionManager,
            rate_key: &RateKey,
            rpm: Option<u32>,
            tpm: Option<u32>,
            tpm_permits: u32,
        ) -> redis::RedisResult<Option<Duration>> {
            let rpm_enabled = if rpm.is_some() { 1 } else { 0 };
            let rpm_per_minute = rpm.unwrap_or(0) as f64;
            let rpm_capacity = rpm_per_minute.max(1.0);
            let rpm_refill_per_second = (rpm_per_minute / 60.0).max(f64::MIN_POSITIVE);

            let tpm_enabled = if tpm.is_some() { 1 } else { 0 };
            let tpm_per_minute = tpm.unwrap_or(0) as f64;
            let tpm_capacity = tpm_per_minute.max(1.0);
            let tpm_refill_per_second = (tpm_per_minute / 60.0).max(f64::MIN_POSITIVE);
            let current_timestamp_ms = current_timestamp_ms();
            let (request_admitted, retry_after_ms): (i64, i64) = self
                .rate_limit_script
                .key(Self::redis_key("rpm", rate_key))
                .key(Self::redis_key("tpm", rate_key))
                .arg(current_timestamp_ms)
                .arg(KEY_TTL_MS)
                .arg(rpm_enabled)
                .arg(rpm_capacity)
                .arg(rpm_refill_per_second)
                .arg(1.0)
                .arg(tpm_enabled)
                .arg(tpm_capacity)
                .arg(tpm_refill_per_second)
                .arg(tpm_permits.max(1) as f64)
                .invoke_async(connection)
                .await?;
            if request_admitted == 1 {
                Ok(None)
            } else {
                Ok(Some(Duration::from_millis(retry_after_ms.max(0) as u64)))
            }
        }
    }

    #[async_trait::async_trait]
    impl RateLimiter for RedisRateLimiter {
        async fn acquire(&self, key: &RateKey, permits: u32) -> Result<(), RetryAfter> {
            let limit = self.config.resolve(&key.provider);
            if limit.is_unlimited() {
                return Ok(());
            }
            if limit.has_zero_limit() {
                return Err(RetryAfter(ZERO_LIMIT_RETRY_AFTER));
            }
            let mut connection = match self.connection().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: redis rate limiter unavailable ({e}); failing open");
                    return Ok(());
                }
            };

            match self
                .check_and_debit(&mut connection, key, limit.rpm, limit.tpm, permits)
                .await
            {
                Ok(Some(wait)) => Err(RetryAfter(wait)),
                Ok(None) => Ok(()),
                Err(error) => {
                    eprintln!("warning: redis rate-limit check failed ({error}); failing open");
                    Ok(())
                }
            }
        }

        async fn penalize(&self, key: &RateKey, retry_after: Duration) {
            let limit = self.config.resolve(&key.provider);
            if limit.is_unlimited() || limit.has_zero_limit() {
                return;
            }
            let mut connection = match self.connection().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: redis penalize skipped, connection failed ({e})");
                    return;
                }
            };
            let penalty_deadline_ms = current_timestamp_ms() + retry_after.as_millis() as u64;
            for bucket_dimension in ["rpm", "tpm"] {
                let _: Result<i64, _> = self
                    .penalty_script
                    .key(Self::redis_key(bucket_dimension, key))
                    .arg(penalty_deadline_ms)
                    .arg(KEY_TTL_MS)
                    .invoke_async(&mut connection)
                    .await;
            }
        }
    }

    fn current_timestamp_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(feature = "redis-coordination")]
pub use redis_impl::RedisRateLimiter;

// ===========================================================================
// Backpressure — per-instance concurrency cap + bounded queue
// ===========================================================================

/// A per-instance cap on in-flight upstream requests, with a bounded wait.
///
/// Beyond the cap, requests queue for a permit up to `queue_timeout`; on timeout
/// the caller should return 503 rather than let memory grow without bound. This
/// is the load-shedding valve that keeps a replica alive at 10k concurrency.
#[derive(Clone)]
pub struct Backpressure {
    semaphore: Arc<Semaphore>,
    queue_timeout: Duration,
}

impl Backpressure {
    pub fn new(max_concurrency: usize, queue_timeout: Duration) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrency.max(1))),
            queue_timeout,
        }
    }

    /// Build from env: `LLMSHIM_MAX_CONCURRENCY` (default 256),
    /// `LLMSHIM_QUEUE_TIMEOUT_MS` (default 5000).
    pub fn from_env() -> Self {
        let max = env_usize("LLMSHIM_MAX_CONCURRENCY").unwrap_or(DEFAULT_MAX_CONCURRENCY);
        let timeout_ms = env_u64("LLMSHIM_QUEUE_TIMEOUT_MS").unwrap_or(DEFAULT_QUEUE_TIMEOUT_MS);
        Self::new(max, Duration::from_millis(timeout_ms))
    }

    pub fn queue_timeout(&self) -> Duration {
        self.queue_timeout
    }

    /// Acquire a permit, waiting at most `queue_timeout`. The permit is held for
    /// the upstream call's lifetime and releases a slot when dropped. `Err(())`
    /// means the queue timed out ⇒ shed load with a 503.
    // The unit error is intentional: the only failure is "timed out / closed",
    // which the caller maps straight to a 503 — no error detail to carry.
    #[allow(clippy::result_unit_err)]
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, ()> {
        match tokio::time::timeout(self.queue_timeout, self.semaphore.clone().acquire_owned()).await
        {
            Ok(Ok(permit)) => Ok(permit),
            // Closed semaphore or timeout — both shed load.
            _ => Err(()),
        }
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

// ===========================================================================
// Token estimation & limiter construction
// ===========================================================================

/// Target-independent token estimate for callers that only have the typed
/// proxy envelope. HTTP admission uses the route-expanded, provider-aware
/// estimator internally.
pub fn estimate_request_tokens(req: &ChatRequest) -> u32 {
    let mut input_chars = serde_json::to_string(&req.messages)
        .map(|messages| messages.len())
        .unwrap_or(0);
    for prompt_bearing_value in [req.provider_config.as_ref(), req.response_format.as_ref()]
        .into_iter()
        .flatten()
    {
        input_chars = input_chars.saturating_add(prompt_bearing_value.to_string().len());
    }
    let input_tokens = u64::try_from(input_chars / 4).unwrap_or(u64::MAX);
    let configured_output_tokens = req.config.as_ref().and_then(|config| config.max_tokens);
    let provider_output_tokens = req
        .provider_config
        .as_ref()
        .and_then(largest_provider_output_budget);
    let output_tokens = configured_output_tokens
        .into_iter()
        .chain(provider_output_tokens)
        .max()
        .unwrap_or(DEFAULT_MAX_TOKENS_ESTIMATE);
    input_tokens
        .saturating_add(output_tokens)
        .clamp(1, u32::MAX as u64) as u32
}

#[cfg(any(feature = "gateway", test))]
pub(crate) fn estimate_prepared_request_tokens(prepared: &PreparedRequest) -> u32 {
    let mut prompt_characters = 0usize;
    for field in ["messages", "tools", "response_format", "x-shim"] {
        if let Some(value) = prepared.payload.get(field) {
            prompt_characters = prompt_characters.saturating_add(value.to_string().len());
        }
    }
    if prepared.target.provider_name == "anthropic" {
        if let Some(output_configuration) = prepared.payload.get("output_config") {
            prompt_characters =
                prompt_characters.saturating_add(output_configuration.to_string().len());
        }
    }
    prompt_characters = prompt_characters.saturating_add(active_native_prompt_characters(prepared));
    let estimated_input_tokens = u64::try_from(prompt_characters / 4).unwrap_or(u64::MAX);
    let estimated_output_tokens = effective_output_budget(prepared);
    estimated_input_tokens
        .saturating_add(estimated_output_tokens)
        .clamp(1, u32::MAX as u64) as u32
}

/// Estimate the final immutable provider-native body seen by the per-attempt
/// policy hook. This is deliberately separate from logical-request admission:
/// retries reuse the same prepared body, while repairs and fallback targets
/// arrive here with their own authoritative wire body and target.
pub(crate) fn estimate_attempt_tokens(attempt: &crate::policy::PreparedAttempt<'_>) -> u32 {
    estimate_native_attempt_tokens(
        attempt.identity().provider_name(),
        attempt.identity().native_model(),
        attempt.identity().wire(),
        attempt.native_body(),
    )
}

fn estimate_native_attempt_tokens(
    provider: &str,
    model: &str,
    wire: crate::reasoning::WireFormat,
    native_body: &serde_json::Value,
) -> u32 {
    let prompt_tokens = serde_json::to_string(native_body)
        .map(|serialized| serialized.len() as u64 / 4)
        .unwrap_or(u64::MAX);
    let explicit_output = match wire {
        crate::reasoning::WireFormat::AnthropicMessages => native_body
            .get("max_tokens")
            .and_then(|value| value.as_u64()),
        crate::reasoning::WireFormat::OpenAiResponses => native_body
            .get("max_output_tokens")
            .and_then(|value| value.as_u64()),
        crate::reasoning::WireFormat::OpenAiChat => native_body
            .get("max_completion_tokens")
            .or_else(|| native_body.get("max_tokens"))
            .and_then(|value| value.as_u64()),
        crate::reasoning::WireFormat::GoogleGenerateContent => native_body
            .pointer("/generationConfig/maxOutputTokens")
            .and_then(|value| value.as_u64()),
    };
    let reasoning_output = match wire {
        crate::reasoning::WireFormat::AnthropicMessages => native_body
            .pointer("/thinking/budget_tokens")
            .and_then(|value| value.as_u64()),
        crate::reasoning::WireFormat::GoogleGenerateContent => native_body
            .pointer("/generationConfig/thinkingConfig/thinkingBudget")
            .and_then(|value| value.as_u64()),
        crate::reasoning::WireFormat::OpenAiResponses
        | crate::reasoning::WireFormat::OpenAiChat => native_body
            .pointer("/reasoning/max_tokens")
            .and_then(|value| value.as_u64()),
    }
    .unwrap_or_default();
    let default_output = omitted_native_output_budget(provider, model, native_body);
    let output_candidate_count = native_output_candidate_count(wire, native_body);
    prompt_tokens
        .saturating_add(
            explicit_output
                .unwrap_or(default_output)
                .max(reasoning_output)
                .saturating_mul(output_candidate_count),
        )
        .clamp(1, u32::MAX as u64) as u32
}

fn native_output_candidate_count(
    wire: crate::reasoning::WireFormat,
    native_body: &serde_json::Value,
) -> u64 {
    let candidate_controls: &[Option<&serde_json::Value>] = match wire {
        crate::reasoning::WireFormat::OpenAiChat => &[
            native_body.get("n"),
            native_body.get("best_of"),
            native_body.get("bestOf"),
        ],
        crate::reasoning::WireFormat::GoogleGenerateContent => &[
            native_body.pointer("/generationConfig/candidateCount"),
            native_body.pointer("/generationConfig/candidate_count"),
        ],
        crate::reasoning::WireFormat::AnthropicMessages
        | crate::reasoning::WireFormat::OpenAiResponses => &[],
    };

    candidate_controls
        .iter()
        .flatten()
        .map(|value| value.as_u64().filter(|count| *count > 0))
        .try_fold(1u64, |maximum_candidate_count, count| {
            count.map(|count| maximum_candidate_count.max(count))
        })
        .unwrap_or(u64::MAX)
}

fn catalog_output_budget(provider: &str, model: &str) -> Option<u64> {
    crate::catalog::lookup_id(&format!("{provider}/{model}"))
        .or_else(|| crate::catalog::lookup_id(model))
        .and_then(|model| model.max_output_tokens)
        .map(u64::from)
}

fn omitted_native_output_budget(
    provider: &str,
    model: &str,
    native_body: &serde_json::Value,
) -> u64 {
    let primary_budget = catalog_output_budget(provider, model).unwrap_or(match provider {
        "anthropic" => 8_192,
        "gemini" => 65_536,
        "openai" | "chatgpt" | "xai" => 128_000,
        "openrouter" => 1_048_576,
        _ => 1_024,
    });
    if provider != "openrouter" {
        return primary_budget;
    }
    native_body
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|routed_model| catalog_output_budget("openrouter", routed_model).unwrap_or(1_048_576))
        .fold(primary_budget, u64::max)
}

#[cfg(any(feature = "gateway", test))]
fn active_native_prompt_characters(prepared: &PreparedRequest) -> usize {
    let Some(active_namespace) = active_native_namespace(&prepared.target) else {
        return 0;
    };
    let Some(native_configuration) = prepared
        .payload
        .get(active_namespace)
        .and_then(serde_json::Value::as_object)
    else {
        return 0;
    };
    prepared
        .target
        .policy
        .native_prompt_fields()
        .iter()
        .fold(0usize, |total, field| {
            total.saturating_add(
                native_configuration
                    .get(*field)
                    .map(|value| value.to_string().len())
                    .unwrap_or_default(),
            )
        })
}

#[cfg(any(feature = "gateway", test))]
fn portable_output_budget(request: &serde_json::Value) -> Option<u64> {
    request
        .get("max_tokens")
        .or_else(|| request.get("max_completion_tokens"))
        .and_then(serde_json::Value::as_u64)
}

#[cfg(any(feature = "gateway", test))]
fn native_u64(request: &serde_json::Value, namespace: &str, path: &[&str]) -> Option<u64> {
    let mut value = request.get(namespace)?;
    for field in path {
        value = value.get(*field)?;
    }
    value.as_u64()
}

#[cfg(any(feature = "gateway", test))]
fn native_direct_output_budget(
    request: &serde_json::Value,
    target: &AdmissionTarget,
) -> Option<u64> {
    let namespace = active_native_namespace(target)?;
    target
        .policy
        .native_output_limit_fields()
        .iter()
        .filter_map(|field| native_u64(request, namespace, &[*field]))
        .max()
}

#[cfg(any(feature = "gateway", test))]
fn reasoning_output_budget(request: &serde_json::Value, target: &AdmissionTarget) -> Option<u64> {
    let namespace = active_native_namespace(target)?;
    match target.provider_name.as_str() {
        "openrouter" | "chatgpt" => native_u64(request, namespace, &["reasoning", "max_tokens"]),
        "anthropic" => {
            if request
                .get(namespace)
                .and_then(|configuration| configuration.get("thinking"))
                .is_some()
            {
                native_u64(request, namespace, &["thinking", "budget_tokens"])
            } else {
                request
                    .pointer("/thinking/budget_tokens")
                    .and_then(serde_json::Value::as_u64)
            }
        }
        "gemini" => native_u64(request, namespace, &["thinkingConfig", "thinkingBudget"]),
        _ => None,
    }
}

#[cfg(any(feature = "gateway", test))]
fn omitted_output_budget(target: &AdmissionTarget) -> u64 {
    if target.provider_name == "anthropic" {
        return 8_192;
    }
    crate::catalog::resolve(&format!("{}/{}", target.provider_name, target.model))
        .or_else(|| crate::catalog::resolve(&target.model))
        .and_then(|model| model.max_output_tokens)
        .map(u64::from)
        .unwrap_or_else(|| match target.provider_name.as_str() {
            "openai" | "chatgpt" | "xai" => 128_000,
            "gemini" => 65_536,
            "openrouter" => 1_048_576,
            _ => DEFAULT_MAX_TOKENS_ESTIMATE,
        })
}

#[cfg(any(feature = "gateway", test))]
fn omitted_prepared_output_budget(prepared: &PreparedRequest) -> u64 {
    let primary_budget = omitted_output_budget(&prepared.target);
    if prepared.target.provider_name != "openrouter" {
        return primary_budget;
    }
    prepared
        .payload
        .pointer("/x-openrouter/models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(|model| {
            crate::catalog::resolve(&format!("openrouter/{model}"))
                .or_else(|| crate::catalog::resolve(model))
                .and_then(|model| model.max_output_tokens)
                .map(u64::from)
                .unwrap_or(1_048_576)
        })
        .fold(primary_budget, u64::max)
}

#[cfg(any(feature = "gateway", test))]
fn effective_output_budget(prepared: &PreparedRequest) -> u64 {
    let requested_output_budget = if prepared.target.provider_name == "chatgpt" {
        None
    } else {
        native_direct_output_budget(&prepared.payload, &prepared.target)
            .or_else(|| portable_output_budget(&prepared.payload))
    }
    .unwrap_or_else(|| omitted_prepared_output_budget(prepared));
    requested_output_budget
        .max(reasoning_output_budget(&prepared.payload, &prepared.target).unwrap_or_default())
}

fn direct_output_budget(config: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    [
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "maxOutputTokens",
    ]
    .into_iter()
    .filter_map(|field| config.get(field).and_then(serde_json::Value::as_u64))
    .max()
}

fn largest_provider_output_budget(provider_config: &serde_json::Value) -> Option<u64> {
    let provider_config = provider_config.as_object()?;
    let mut largest_budget = direct_output_budget(provider_config);
    for (field, native_config) in provider_config {
        if !field.starts_with("x-") {
            continue;
        }
        let Some(native_config) = native_config.as_object() else {
            continue;
        };
        largest_budget = largest_budget.max(direct_output_budget(native_config));
        if let Some(generation_config) = native_config
            .get("generationConfig")
            .and_then(serde_json::Value::as_object)
        {
            largest_budget = largest_budget.max(direct_output_budget(generation_config));
        }
    }
    largest_budget
}

/// Build the configured limiter from the environment.
///
/// If `LLMSHIM_REDIS_URL` is set and the `redis-coordination` feature is
/// compiled in, use the distributed limiter; otherwise (or on init failure) fall
/// back to the in-memory limiter, logging a clear warning.
pub fn build_limiter() -> Arc<dyn RateLimiter> {
    let config = RateLimitConfig::from_env();

    if let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") {
        let url = url.trim().to_string();
        if !url.is_empty() {
            #[cfg(feature = "redis-coordination")]
            {
                match RedisRateLimiter::new(&url, config.clone()) {
                    Ok(limiter) => {
                        eprintln!("rate limiting: redis coordination enabled ({url})");
                        return Arc::new(limiter);
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: LLMSHIM_REDIS_URL set but redis client init failed ({e}); \
                             falling back to in-memory rate limiting"
                        );
                    }
                }
            }
            #[cfg(not(feature = "redis-coordination"))]
            {
                eprintln!(
                    "warning: LLMSHIM_REDIS_URL is set but this binary was built without the \
                     'redis-coordination' feature; using in-memory rate limiting. Rebuild with \
                     --features redis-coordination for distributed coordination."
                );
            }
        }
    }

    Arc::new(InMemoryRateLimiter::new(config))
}

// ===========================================================================
// Env helpers
// ===========================================================================

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse().ok()
}

// ===========================================================================
// Tests — all pure / in-process. No network, no external services, ~$0.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    // --- TokenBucket math (deterministic via tokio::time) -------------------

    #[tokio::test(start_paused = true)]
    async fn bucket_starts_full_and_acquire_depletes() {
        let now = Instant::now();
        let mut b = TokenBucket::from_per_minute(60.0, now); // 60/min = 1/s, burst 60
                                                             // Drain all 60 tokens.
        for _ in 0..60 {
            assert!(b.check(1.0, now).is_ok());
            b.commit(1.0);
        }
        // Now empty → exhaustion returns a positive wait (~1s to refill 1 token).
        let wait = b.check(1.0, now).unwrap_err();
        assert!(
            wait > Duration::ZERO && wait <= secs(1.01),
            "wait was {wait:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_refills_over_time() {
        let start = Instant::now();
        let mut b = TokenBucket::from_per_minute(60.0, start); // 1 token/sec
        for _ in 0..60 {
            b.commit(1.0);
        }
        assert!(b.check(1.0, start).is_err());

        // Advance 5 virtual seconds → ~5 tokens refilled.
        tokio::time::advance(secs(5.0)).await;
        let now = Instant::now();
        b.refill(now);
        for _ in 0..5 {
            assert!(b.check(1.0, now).is_ok(), "should have refilled 5 tokens");
            b.commit(1.0);
        }
        assert!(b.check(1.0, now).is_err(), "6th token not yet refilled");
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_refill_caps_at_capacity() {
        let start = Instant::now();
        let mut b = TokenBucket::from_per_minute(60.0, start);
        b.commit(60.0); // empty
        tokio::time::advance(secs(3600.0)).await; // an hour
        let now = Instant::now();
        b.refill(now);
        assert!(b.tokens <= b.capacity + 1e-6);
        assert!((b.tokens - 60.0).abs() < 1e-6, "capped at capacity");
    }

    #[tokio::test(start_paused = true)]
    async fn bucket_exhaustion_wait_scales_with_deficit() {
        let now = Instant::now();
        let mut b = TokenBucket::from_per_minute(60.0, now); // 1/sec
        b.commit(60.0);
        // Need 10 tokens, refill is 1/sec ⇒ ~10s.
        let wait = b.check(10.0, now).unwrap_err();
        assert!(wait >= secs(9.9) && wait <= secs(10.1), "wait {wait:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_bucket_rejects_with_finite_wait_without_panicking() {
        let now = Instant::now();
        let bucket = TokenBucket::from_per_minute(0.0, now);
        assert_eq!(
            bucket.check(1.0, now),
            Err(ZERO_LIMIT_RETRY_AFTER),
            "zero refill must be a finite policy rejection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn penalize_backs_off_globally() {
        let now = Instant::now();
        let mut b = TokenBucket::from_per_minute(600.0, now); // plenty of tokens
        assert!(b.check(1.0, now).is_ok());

        b.penalize(secs(30.0), now);
        // Even though tokens would allow it, penalty window denies for ~30s.
        let wait = b.check(1.0, now).unwrap_err();
        assert!(wait > secs(29.0) && wait <= secs(30.01), "wait {wait:?}");

        // After the window elapses, admits again.
        tokio::time::advance(secs(31.0)).await;
        let later = Instant::now();
        b.refill(later);
        assert!(b.check(1.0, later).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn penalize_takes_the_later_deadline() {
        let now = Instant::now();
        let mut b = TokenBucket::from_per_minute(600.0, now);
        b.penalize(secs(10.0), now);
        b.penalize(secs(5.0), now); // shorter — must not shrink the window
        let wait = b.check(1.0, now).unwrap_err();
        assert!(wait > secs(9.0), "later deadline kept, wait {wait:?}");
    }

    // --- KeyBuckets: RPM + TPM together -------------------------------------

    #[tokio::test(start_paused = true)]
    async fn keybuckets_charges_both_or_neither() {
        let now = Instant::now();
        let mut kb = KeyBuckets {
            rpm: Some(TokenBucket::from_per_minute(600.0, now)), // lots of requests
            tpm: Some(TokenBucket::from_per_minute(100.0, now)), // few tokens
        };
        // First request wanting 100 tokens: ok (tpm exactly 100 capacity).
        assert!(kb.acquire(100.0, now).is_ok());
        // Second wanting 100: tpm empty → denied, and RPM must NOT have been charged.
        let before = kb.rpm.as_ref().unwrap().tokens;
        assert!(kb.acquire(100.0, now).is_err());
        let after = kb.rpm.as_ref().unwrap().tokens;
        assert!(
            (before - after).abs() < 1e-9,
            "rpm charged despite tpm denial"
        );
    }

    // --- RateLimitConfig ----------------------------------------------------

    #[test]
    fn config_resolve_per_provider_overrides_global_per_field() {
        let mut per = HashMap::new();
        per.insert(
            "openai".to_string(),
            ProviderLimit {
                rpm: Some(10),
                tpm: None,
            },
        );
        let cfg = RateLimitConfig {
            global: ProviderLimit {
                rpm: Some(100),
                tpm: Some(1000),
            },
            per_provider: per,
        };
        let openai = cfg.resolve("openai");
        assert_eq!(openai.rpm, Some(10)); // override
        assert_eq!(openai.tpm, Some(1000)); // inherited global
        let other = cfg.resolve("anthropic");
        assert_eq!(other.rpm, Some(100));
        assert_eq!(other.tpm, Some(1000));
    }

    #[test]
    fn config_default_is_unlimited() {
        let cfg = RateLimitConfig::default();
        assert!(cfg.is_unlimited());
        assert!(cfg.resolve("openai").is_unlimited());
    }

    // --- InMemoryRateLimiter ------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn inmemory_noop_when_unlimited() {
        let limiter = InMemoryRateLimiter::new(RateLimitConfig::default());
        let key = RateKey::provider("openai");
        // No limits ⇒ always admits, forever.
        for _ in 0..10_000 {
            assert!(limiter.acquire(&key, 1000).await.is_ok());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inmemory_enforces_rpm_then_recovers() {
        let cfg = RateLimitConfig {
            global: ProviderLimit {
                rpm: Some(60),
                tpm: None,
            },
            per_provider: HashMap::new(),
        };
        let limiter = InMemoryRateLimiter::new(cfg);
        let key = RateKey::provider("openai");

        // Burst of 60 admitted.
        for _ in 0..60 {
            assert!(limiter.acquire(&key, 1).await.is_ok());
        }
        // 61st denied with a positive Retry-After.
        let err = limiter.acquire(&key, 1).await.unwrap_err();
        assert!(err.0 > Duration::ZERO);

        // After a virtual minute, the bucket has refilled.
        tokio::time::advance(secs(60.0)).await;
        assert!(limiter.acquire(&key, 1).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn inmemory_zero_rpm_or_tpm_rejects_without_partial_debit() {
        for limit in [
            ProviderLimit {
                rpm: Some(0),
                tpm: Some(100),
            },
            ProviderLimit {
                rpm: Some(100),
                tpm: Some(0),
            },
        ] {
            for config in [
                RateLimitConfig {
                    global: limit,
                    per_provider: HashMap::new(),
                },
                RateLimitConfig {
                    global: ProviderLimit {
                        rpm: Some(100),
                        tpm: Some(100),
                    },
                    per_provider: HashMap::from([(String::from("openai"), limit)]),
                },
            ] {
                let limiter = InMemoryRateLimiter::new(config);
                let key = RateKey::provider("openai");

                for _ in 0..100 {
                    let error = limiter.acquire(&key, 1).await.unwrap_err();
                    assert_eq!(error.0, ZERO_LIMIT_RETRY_AFTER);
                }
                assert!(
                    limiter.buckets.lock().await.is_empty(),
                    "zero limit must reject before creating or charging buckets"
                );
            }
        }
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    async fn redis_zero_limit_rejects_before_connecting_or_debiting() {
        let loopback_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let loopback_address = loopback_listener.local_addr().unwrap();
        let redis_rate_limiter = RedisRateLimiter::new(
            &format!("redis://{loopback_address}"),
            RateLimitConfig::with_global(Some(100), Some(0)),
        )
        .expect("valid Redis URL");
        let acquisition_result = tokio::time::timeout(
            Duration::from_secs(1),
            redis_rate_limiter.acquire(&RateKey::provider("test-provider"), 1),
        )
        .await
        .expect("zero limit must reject without waiting for Redis");
        assert_eq!(acquisition_result.unwrap_err().0, ZERO_LIMIT_RETRY_AFTER);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), loopback_listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inmemory_penalize_denies_until_window_clears() {
        let cfg = RateLimitConfig {
            global: ProviderLimit {
                rpm: Some(600),
                tpm: None,
            },
            per_provider: HashMap::new(),
        };
        let limiter = InMemoryRateLimiter::new(cfg);
        let key = RateKey::provider("openai");
        assert!(limiter.acquire(&key, 1).await.is_ok());

        limiter.penalize(&key, secs(10.0)).await;
        let err = limiter.acquire(&key, 1).await.unwrap_err();
        assert!(err.0 > secs(9.0));

        tokio::time::advance(secs(11.0)).await;
        assert!(limiter.acquire(&key, 1).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn inmemory_keys_are_independent_per_provider() {
        let cfg = RateLimitConfig {
            global: ProviderLimit {
                rpm: Some(1),
                tpm: None,
            },
            per_provider: HashMap::new(),
        };
        let limiter = InMemoryRateLimiter::new(cfg);
        let openai = RateKey::provider("openai");
        let anthropic = RateKey::provider("anthropic");
        assert!(limiter.acquire(&openai, 1).await.is_ok());
        assert!(limiter.acquire(&openai, 1).await.is_err()); // openai exhausted
        assert!(limiter.acquire(&anthropic, 1).await.is_ok()); // anthropic independent
    }

    #[cfg(feature = "redis-coordination")]
    #[tokio::test]
    #[ignore = "requires an isolated Redis at LLMSHIM_REDIS_URL"]
    async fn redis_oversized_tpm_rejection_does_not_debit_rpm() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL")
            .expect("the ignored integration test requires LLMSHIM_REDIS_URL");
        let unique_provider = format!(
            "atomic-rate-debits-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        );
        let limiter =
            RedisRateLimiter::new(&redis_url, RateLimitConfig::with_global(Some(1), Some(10)))
                .expect("valid Redis URL");
        let key = RateKey::provider(unique_provider);

        assert!(
            limiter.acquire(&key, 11).await.is_err(),
            "request exceeding TPM must be rejected"
        );
        assert!(
            limiter.acquire(&key, 1).await.is_ok(),
            "rejected oversized work must not consume shared RPM capacity"
        );
        assert!(
            limiter.acquire(&key, 1).await.is_err(),
            "the admitted request consumes the configured RPM capacity"
        );
    }

    // --- Backpressure: concurrency cap + queue timeout ----------------------

    #[tokio::test(start_paused = true)]
    async fn backpressure_admits_up_to_cap() {
        let bp = Backpressure::new(2, secs(1.0));
        let p1 = bp.acquire().await.expect("first permit");
        let _p2 = bp.acquire().await.expect("second permit");
        assert_eq!(bp.available_permits(), 0);
        // Releasing frees a slot.
        drop(p1);
        assert_eq!(bp.available_permits(), 1);
        assert!(bp.acquire().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn backpressure_times_out_beyond_cap() {
        let bp = Backpressure::new(1, Duration::from_millis(200));
        let _held = bp.acquire().await.expect("permit");
        // Second acquire has no slot; must time out (→ 503 at the handler).
        let start = Instant::now();
        let result = bp.acquire().await;
        let waited = start.elapsed();
        assert!(result.is_err(), "should time out when cap is exhausted");
        assert!(waited >= Duration::from_millis(200), "waited {waited:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn backpressure_permit_frees_within_timeout() {
        let bp = Backpressure::new(1, secs(5.0));
        let held = bp.acquire().await.expect("permit");
        // A waiter that gets the permit once `held` is dropped.
        let bp2 = bp.clone();
        let waiter = tokio::spawn(async move { bp2.acquire().await.is_ok() });
        // Let the waiter park, then release.
        tokio::time::advance(Duration::from_millis(100)).await;
        drop(held);
        assert!(waiter.await.unwrap(), "waiter should acquire after release");
    }

    // --- Token estimation ---------------------------------------------------

    #[test]
    fn estimate_uses_content_and_max_tokens() {
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-5.5",
            "messages": [{"role": "user", "content": "hello world this is a test"}],
            "config": {"max_tokens": 500}
        }))
        .unwrap();
        let est = estimate_request_tokens(&req);
        // ~ (len/4) input + 500 output; must exceed the output budget.
        assert!(est >= 500);
    }

    #[test]
    fn estimate_defaults_output_when_absent() {
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let est = estimate_request_tokens(&req);
        assert!(est >= DEFAULT_MAX_TOKENS_ESTIMATE as u32);
    }

    #[test]
    fn estimate_handles_array_content_blocks() {
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "some words here"}
            ]}],
            "config": {"max_tokens": 10}
        }))
        .unwrap();
        // Should not panic and should include output budget.
        assert!(estimate_request_tokens(&req) >= 10);
    }

    #[test]
    fn estimate_counts_tool_schemas_and_namespaced_output_limits() {
        let large_description = "x".repeat(8_000);
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-5.6-luna",
            "messages": [{"role": "user", "content": "hi"}],
            "config": {"max_tokens": 10},
            "provider_config": {
                "tools": [{"type": "function", "function": {
                    "name": "lookup",
                    "description": large_description,
                    "parameters": {"type": "object", "properties": {}}
                }}],
                "x-openai": {"max_output_tokens": 4_000}
            }
        }))
        .unwrap();
        assert!(
            estimate_request_tokens(&req) >= 6_000,
            "the native output limit and serialized tool schema must both consume permits"
        );
    }

    #[test]
    fn estimate_counts_namespaced_native_prompt_content() {
        let native_input = "x".repeat(8_000);
        let req: ChatRequest = serde_json::from_value(serde_json::json!({
            "model": "openai/gpt-5.6-luna",
            "messages": [{"role": "user", "content": "hi"}],
            "provider_config": {"x-openai": {"input": native_input}}
        }))
        .unwrap();
        assert!(estimate_request_tokens(&req) >= 3_000);
    }

    fn prepared_request(
        provider_name: &str,
        model: &str,
        wire: crate::reasoning::WireFormat,
        payload: serde_json::Value,
    ) -> PreparedRequest {
        let policy = match provider_name {
            "openai" => crate::provider::RequestAdmissionPolicy::namespaced(
                "x-openai",
                &["model", "input"],
                &["instructions", "prompt", "text", "tools", "tool_choice"],
                &["max_output_tokens"],
            ),
            "chatgpt" => crate::provider::RequestAdmissionPolicy::namespaced(
                "x-chatgpt",
                &["input"],
                &["instructions", "text", "tools", "tool_choice", "reasoning"],
                &[],
            ),
            "anthropic" => crate::provider::RequestAdmissionPolicy::namespaced(
                "x-anthropic",
                &["model", "messages"],
                &[
                    "system",
                    "tools",
                    "tool_choice",
                    "thinking",
                    "output_config",
                ],
                &["max_tokens"],
            ),
            "gemini" => crate::provider::RequestAdmissionPolicy::namespaced(
                "x-gemini",
                &["contents"],
                &["systemInstruction", "tools", "toolConfig", "thinkingConfig"],
                &[],
            ),
            "openrouter" => crate::provider::RequestAdmissionPolicy::namespaced(
                "x-openrouter",
                &["model", "messages"],
                &["tools", "tool_choice", "response_format", "reasoning"],
                &["max_tokens", "max_completion_tokens"],
            ),
            "vllm" | "sglang" => match wire {
                crate::reasoning::WireFormat::OpenAiResponses => {
                    crate::provider::RequestAdmissionPolicy::namespaced(
                        format!("x-{provider_name}"),
                        &["model", "input"],
                        &["instructions", "tools", "tool_choice"],
                        &["max_output_tokens"],
                    )
                }
                _ => crate::provider::RequestAdmissionPolicy::namespaced(
                    format!("x-{provider_name}"),
                    &["model", "messages"],
                    &["tools", "tool_choice", "response_format"],
                    &["max_tokens", "max_completion_tokens"],
                ),
            },
            _ => crate::provider::RequestAdmissionPolicy::default(),
        };
        PreparedRequest {
            payload,
            target: AdmissionTarget {
                provider_name: provider_name.into(),
                model: model.into(),
                policy,
            },
        }
    }

    #[test]
    fn prepared_estimate_uses_provider_defaults_and_nested_reasoning_budgets() {
        let anthropic = prepared_request(
            "anthropic",
            "claude-sonnet-5",
            crate::reasoning::WireFormat::AnthropicMessages,
            serde_json::json!({"model":"anthropic/claude-sonnet-5","messages":[]}),
        );
        assert_eq!(effective_output_budget(&anthropic), 8_192);

        let openrouter = prepared_request(
            "openrouter",
            "anthropic/claude-sonnet-5",
            crate::reasoning::WireFormat::OpenAiChat,
            serde_json::json!({
                "model":"openrouter/anthropic/claude-sonnet-5","messages":[],
                "max_tokens": 500,
                "x-openrouter":{"reasoning":{"max_tokens":12_000}}
            }),
        );
        assert_eq!(effective_output_budget(&openrouter), 12_000);
        let openrouter_routing = prepared_request(
            "openrouter",
            "anthropic/claude-sonnet-5",
            crate::reasoning::WireFormat::OpenAiChat,
            serde_json::json!({
                "model":"openrouter/anthropic/claude-sonnet-5","messages":[],
                "x-openrouter":{"models":["unknown/provider-model"]}
            }),
        );
        assert_eq!(effective_output_budget(&openrouter_routing), 1_048_576);

        let anthropic_thinking = prepared_request(
            "anthropic",
            "claude-sonnet-5",
            crate::reasoning::WireFormat::AnthropicMessages,
            serde_json::json!({
                "model":"anthropic/claude-sonnet-5","messages":[],"max_tokens":4_000,
                "thinking":{"type":"enabled","budget_tokens":3_000},
                "x-anthropic":{"thinking":{"type":"enabled","budget_tokens":6_000}}
            }),
        );
        assert_eq!(effective_output_budget(&anthropic_thinking), 6_000);

        let gemini = prepared_request(
            "gemini",
            "gemini-3.8-flash",
            crate::reasoning::WireFormat::GoogleGenerateContent,
            serde_json::json!({
                "model":"gemini/gemini-3.8-flash","messages":[],"max_tokens":1_000,
                "x-gemini":{"thinkingConfig":{"thinkingBudget":9_000}}
            }),
        );
        assert_eq!(effective_output_budget(&gemini), 9_000);
    }

    #[test]
    fn prepared_estimate_ignores_inactive_namespaces_and_schema_property_names() {
        let baseline = prepared_request(
            "openai",
            "gpt-5.6-luna",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({
                "model":"openai/gpt-5.6-luna","messages":[],"max_tokens":200,
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{
                    "type":"object","properties":{"max_tokens":{"const":u64::MAX},"input":{"type":"string"}}
                }}}]
            }),
        );
        let with_inactive_namespace = prepared_request(
            "openai",
            "gpt-5.6-luna",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({
                "model":"openai/gpt-5.6-luna","messages":[],"max_tokens":200,
                "tools":[{"type":"function","function":{"name":"lookup","parameters":{
                    "type":"object","properties":{"max_tokens":{"const":u64::MAX},"input":{"type":"string"}}
                }}}],
                "x-anthropic":{"max_tokens":u64::MAX,"input":"x".repeat(8_000)},
                "x-unknown":{"max_output_tokens":u64::MAX}
            }),
        );
        assert_eq!(effective_output_budget(&baseline), 200);
        assert_eq!(effective_output_budget(&with_inactive_namespace), 200);
        assert_eq!(
            estimate_prepared_request_tokens(&baseline),
            estimate_prepared_request_tokens(&with_inactive_namespace)
        );

        let active_instructions = prepared_request(
            "openai",
            "gpt-5.6-luna",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({
                "model":"openai/gpt-5.6-luna","messages":[],"max_tokens":200,
                "x-openai":{"instructions":"x".repeat(8_000)}
            }),
        );
        assert!(
            estimate_prepared_request_tokens(&active_instructions)
                >= estimate_prepared_request_tokens(&baseline).saturating_add(1_900)
        );
    }

    #[test]
    fn prepared_estimate_counts_compact_anthropic_output_schemas() {
        let baseline = prepared_request(
            "anthropic",
            "claude-sonnet-5",
            crate::reasoning::WireFormat::AnthropicMessages,
            serde_json::json!({
                "model":"anthropic/claude-sonnet-5","messages":[],"max_tokens":100
            }),
        );
        let root_schema = prepared_request(
            "anthropic",
            "claude-sonnet-5",
            crate::reasoning::WireFormat::AnthropicMessages,
            serde_json::json!({
                "model":"anthropic/claude-sonnet-5","messages":[],"max_tokens":100,
                "output_config":{"format":{"type":"json_schema","schema":{
                    "type":"object","description":"x".repeat(8_000)
                }}}
            }),
        );
        let namespaced_schema = prepared_request(
            "anthropic",
            "claude-sonnet-5",
            crate::reasoning::WireFormat::AnthropicMessages,
            serde_json::json!({
                "model":"anthropic/claude-sonnet-5","messages":[],"max_tokens":100,
                "x-anthropic":{"output_config":{"format":{"type":"json_schema","schema":{
                    "type":"object","description":"x".repeat(8_000)
                }}}}
            }),
        );
        let baseline_permits = estimate_prepared_request_tokens(&baseline);
        assert!(
            estimate_prepared_request_tokens(&root_schema)
                >= baseline_permits.saturating_add(1_900)
        );
        assert!(
            estimate_prepared_request_tokens(&namespaced_schema)
                >= baseline_permits.saturating_add(1_900)
        );
    }

    #[test]
    fn prepared_estimate_tracks_chatgpt_and_self_hosted_wire_semantics() {
        let openai = prepared_request(
            "openai",
            "gpt-5.6-luna",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({"model":"openai/gpt-5.6-luna","messages":[]}),
        );
        assert_eq!(effective_output_budget(&openai), 128_000);
        let unknown_openai_target = AdmissionTarget {
            provider_name: "openai".into(),
            model: "gpt-unknown".into(),
            policy: crate::provider::RequestAdmissionPolicy::namespaced(
                "x-openai",
                &["model", "input"],
                &["instructions"],
                &["max_output_tokens"],
            ),
        };
        assert_eq!(omitted_output_budget(&unknown_openai_target), 128_000);

        let chatgpt = prepared_request(
            "chatgpt",
            "gpt-5.6-luna",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({
                "model":"chatgpt/gpt-5.6-luna","messages":[],
                "max_tokens":1,"x-chatgpt":{"max_output_tokens":2}
            }),
        );
        assert_eq!(
            effective_output_budget(&chatgpt),
            omitted_output_budget(&chatgpt.target)
        );

        let vllm_chat = prepared_request(
            "vllm",
            "served",
            crate::reasoning::WireFormat::OpenAiChat,
            serde_json::json!({
                "model":"vllm/served","messages":[],"max_tokens":100,
                "x-vllm":{"max_tokens":7_000}
            }),
        );
        assert_eq!(effective_output_budget(&vllm_chat), 7_000);

        let sglang_responses = prepared_request(
            "sglang",
            "served",
            crate::reasoning::WireFormat::OpenAiResponses,
            serde_json::json!({
                "model":"sglang/served","messages":[],"max_tokens":100,
                "x-sglang":{"max_output_tokens":8_000}
            }),
        );
        assert_eq!(effective_output_budget(&sglang_responses), 8_000);
    }

    #[test]
    fn native_attempt_estimate_uses_authoritative_wire_fields() {
        let openai = estimate_native_attempt_tokens(
            "openai",
            "unknown-test-model",
            crate::reasoning::WireFormat::OpenAiResponses,
            &serde_json::json!({
                "model": "unknown-test-model",
                "input": "short",
                "max_output_tokens": 7_000,
                "max_tokens": 1
            }),
        );
        let anthropic = estimate_native_attempt_tokens(
            "anthropic",
            "unknown-test-model",
            crate::reasoning::WireFormat::AnthropicMessages,
            &serde_json::json!({
                "model": "unknown-test-model",
                "messages": [{"role": "user", "content": "short"}],
                "max_tokens": 2_000,
                "thinking": {"budget_tokens": 6_000}
            }),
        );
        let gemini = estimate_native_attempt_tokens(
            "gemini",
            "unknown-test-model",
            crate::reasoning::WireFormat::GoogleGenerateContent,
            &serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "short"}]}],
                "generationConfig": {
                    "maxOutputTokens": 3_000,
                    "thinkingConfig": {"thinkingBudget": 5_000}
                }
            }),
        );

        assert!(openai >= 7_000);
        assert!(anthropic >= 6_000);
        assert!(gemini >= 5_000);
    }

    #[test]
    fn native_attempt_estimate_counts_final_wire_output_candidates() {
        let openai_body = serde_json::json!({
            "model": "served",
            "messages": [],
            "max_tokens": 100,
            "n": 2,
            "best_of": 3,
        });
        let openai_prompt_tokens = serde_json::to_string(&openai_body).unwrap().len() as u64 / 4;
        assert_eq!(
            estimate_native_attempt_tokens(
                "vllm",
                "served",
                crate::reasoning::WireFormat::OpenAiChat,
                &openai_body,
            ),
            openai_prompt_tokens.saturating_add(300) as u32,
        );

        let gemini_body = serde_json::json!({
            "contents": [],
            "generationConfig": {"maxOutputTokens": 100, "candidateCount": 2},
        });
        let gemini_prompt_tokens = serde_json::to_string(&gemini_body).unwrap().len() as u64 / 4;
        assert_eq!(
            estimate_native_attempt_tokens(
                "gemini",
                "served",
                crate::reasoning::WireFormat::GoogleGenerateContent,
                &gemini_body,
            ),
            gemini_prompt_tokens.saturating_add(200) as u32,
        );
    }

    #[test]
    fn native_attempt_candidate_controls_fail_closed_without_walking_tool_schemas() {
        for candidate_control in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("2"),
            serde_json::json!(true),
        ] {
            let body = serde_json::json!({
                "model": "served",
                "messages": [],
                "max_tokens": 1,
                "n": candidate_control,
            });
            assert_eq!(
                estimate_native_attempt_tokens(
                    "vllm",
                    "served",
                    crate::reasoning::WireFormat::OpenAiChat,
                    &body,
                ),
                u32::MAX,
            );
        }

        let overflow_body = serde_json::json!({
            "model": "served",
            "messages": [],
            "max_tokens": 2,
            "n": u64::MAX,
        });
        assert_eq!(
            estimate_native_attempt_tokens(
                "vllm",
                "served",
                crate::reasoning::WireFormat::OpenAiChat,
                &overflow_body,
            ),
            u32::MAX,
        );

        let tool_schema_body = serde_json::json!({
            "model": "served",
            "messages": [],
            "max_tokens": 2,
            "tools": [{"function": {"parameters": {"properties": {"n": {"default": 99}}}}}],
        });
        let tool_schema_prompt_tokens =
            serde_json::to_string(&tool_schema_body).unwrap().len() as u64 / 4;
        assert_eq!(
            estimate_native_attempt_tokens(
                "vllm",
                "served",
                crate::reasoning::WireFormat::OpenAiChat,
                &tool_schema_body,
            ),
            tool_schema_prompt_tokens.saturating_add(2) as u32,
        );
    }

    #[test]
    fn native_openrouter_routing_uses_every_final_body_model_ceiling() {
        let unknown_routing_body = serde_json::json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [{"role": "user", "content": "short"}],
            "models": ["anthropic/claude-sonnet-5", "vendor/unknown-model"]
        });
        let serialized_prompt_tokens =
            serde_json::to_string(&unknown_routing_body).unwrap().len() as u64 / 4;
        assert_eq!(
            estimate_native_attempt_tokens(
                "openrouter",
                "anthropic/claude-sonnet-5",
                crate::reasoning::WireFormat::OpenAiChat,
                &unknown_routing_body,
            ),
            serialized_prompt_tokens.saturating_add(1_048_576) as u32
        );

        let base_budget = catalog_output_budget("openrouter", "openai/gpt-oss-20b")
            .expect("vendored OpenRouter model metadata");
        assert_eq!(
            catalog_output_budget("openrouter", "openai/gpt-oss-20b:nitro"),
            Some(base_budget),
            "known OpenRouter variants must use their base model metadata"
        );

        let explicit_limit_body = serde_json::json!({
            "model": "anthropic/claude-sonnet-5",
            "messages": [],
            "models": ["vendor/unknown-model"],
            "max_tokens": 4_096
        });
        let explicit_prompt_tokens =
            serde_json::to_string(&explicit_limit_body).unwrap().len() as u64 / 4;
        assert_eq!(
            estimate_native_attempt_tokens(
                "openrouter",
                "anthropic/claude-sonnet-5",
                crate::reasoning::WireFormat::OpenAiChat,
                &explicit_limit_body,
            ),
            explicit_prompt_tokens.saturating_add(4_096) as u32,
            "an explicit final-body limit remains authoritative"
        );
    }

    // --- Trait object dispatch ----------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn works_through_trait_object() {
        let limiter: Arc<dyn RateLimiter> =
            Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default()));
        let key = RateKey::provider("openai");
        assert!(limiter.acquire(&key, 1).await.is_ok());
        limiter.penalize(&key, secs(1.0)).await; // no-op path, must not panic
    }
}
