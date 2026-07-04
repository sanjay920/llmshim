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
        Self {
            capacity: per_minute.max(1.0),
            refill_per_sec: (per_minute / 60.0).max(f64::MIN_POSITIVE),
            tokens: per_minute.max(1.0),
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
        if limit.is_unlimited() {
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

    /// Atomic token-bucket step in a single round-trip. Stores `tokens`, last
    /// refill `ts` (ms) and a `penalty` deadline (ms) in one hash. Returns
    /// `{allowed, wait_ms}`.
    const BUCKET_LUA: &str = r#"
        local cap    = tonumber(ARGV[1])
        local refill = tonumber(ARGV[2])
        local now    = tonumber(ARGV[3])
        local want   = tonumber(ARGV[4])
        local ttl    = tonumber(ARGV[5])

        local h = redis.call('HMGET', KEYS[1], 'tokens', 'ts', 'penalty')
        local tokens = tonumber(h[1])
        local ts = tonumber(h[2])
        local penalty = tonumber(h[3]) or 0
        if tokens == nil then tokens = cap end
        if ts == nil then ts = now end

        local elapsed = now - ts
        if elapsed < 0 then elapsed = 0 end
        tokens = math.min(cap, tokens + (elapsed / 1000.0) * refill)
        ts = now

        local allowed = 0
        local wait = 0
        if penalty > now then
            wait = penalty - now
        elseif tokens >= want then
            tokens = tokens - want
            allowed = 1
        else
            local deficit = want - tokens
            wait = math.ceil(deficit / refill * 1000.0)
        end

        redis.call('HSET', KEYS[1], 'tokens', tokens, 'ts', ts, 'penalty', penalty)
        redis.call('PEXPIRE', KEYS[1], ttl)
        return {allowed, wait}
    "#;

    const PENALTY_LUA: &str = r#"
        local until_ms = tonumber(ARGV[1])
        local ttl = tonumber(ARGV[2])
        redis.call('HSET', KEYS[1], 'penalty', until_ms, 'tokens', 0)
        redis.call('PEXPIRE', KEYS[1], ttl)
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
        conn: OnceCell<ConnectionManager>,
        config: RateLimitConfig,
        bucket_script: redis::Script,
        penalty_script: redis::Script,
    }

    impl RedisRateLimiter {
        /// Parse the URL and build the client. The async connection is
        /// established lazily on first use, so this stays synchronous and can be
        /// called from the non-async `app()` builder.
        pub fn new(url: &str, config: RateLimitConfig) -> redis::RedisResult<Self> {
            Ok(Self {
                client: redis::Client::open(url)?,
                conn: OnceCell::new(),
                config,
                bucket_script: redis::Script::new(BUCKET_LUA),
                penalty_script: redis::Script::new(PENALTY_LUA),
            })
        }

        async fn connection(&self) -> redis::RedisResult<ConnectionManager> {
            self.conn
                .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
                .await
                .cloned()
        }

        fn redis_key(kind: &str, key: &RateKey) -> String {
            format!("llmshim:rl:{}:{}", key.as_str(), kind)
        }

        /// Run the bucket script for one dimension. `Ok(None)` = admitted,
        /// `Ok(Some(wait))` = denied, `Err` = redis error (caller fails open).
        async fn step(
            &self,
            conn: &mut ConnectionManager,
            kind: &str,
            key: &RateKey,
            per_minute: u32,
            want: f64,
        ) -> redis::RedisResult<Option<Duration>> {
            let cap = (per_minute as f64).max(1.0);
            let refill = (per_minute as f64 / 60.0).max(f64::MIN_POSITIVE);
            let now_ms = now_ms();
            let (allowed, wait_ms): (i64, i64) = self
                .bucket_script
                .key(Self::redis_key(kind, key))
                .arg(cap)
                .arg(refill)
                .arg(now_ms)
                .arg(want)
                .arg(KEY_TTL_MS)
                .invoke_async(conn)
                .await?;
            if allowed == 1 {
                Ok(None)
            } else {
                Ok(Some(Duration::from_millis(wait_ms.max(0) as u64)))
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
            let mut conn = match self.connection().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: redis rate limiter unavailable ({e}); failing open");
                    return Ok(());
                }
            };

            let mut wait: Option<Duration> = None;
            if let Some(rpm) = limit.rpm {
                match self.step(&mut conn, "rpm", key, rpm, 1.0).await {
                    Ok(Some(w)) => wait = Some(wait.map_or(w, |c| c.max(w))),
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("warning: redis rpm check failed ({e}); failing open");
                        return Ok(());
                    }
                }
            }
            if let Some(tpm) = limit.tpm {
                let want = (permits.max(1)) as f64;
                match self.step(&mut conn, "tpm", key, tpm, want).await {
                    Ok(Some(w)) => wait = Some(wait.map_or(w, |c| c.max(w))),
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("warning: redis tpm check failed ({e}); failing open");
                        return Ok(());
                    }
                }
            }

            match wait {
                Some(w) => Err(RetryAfter(w)),
                None => Ok(()),
            }
        }

        async fn penalize(&self, key: &RateKey, retry_after: Duration) {
            let limit = self.config.resolve(&key.provider);
            if limit.is_unlimited() {
                return;
            }
            let mut conn = match self.connection().await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: redis penalize skipped, connection failed ({e})");
                    return;
                }
            };
            let until = now_ms() + retry_after.as_millis() as u64;
            for kind in ["rpm", "tpm"] {
                let _: Result<i64, _> = self
                    .penalty_script
                    .key(Self::redis_key(kind, key))
                    .arg(until)
                    .arg(KEY_TTL_MS)
                    .invoke_async(&mut conn)
                    .await;
            }
        }
    }

    fn now_ms() -> u64 {
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

/// Rough token footprint of a request, for the TPM bucket. Not exact — a
/// conservative-ish `chars / 4` over message content plus the requested (or
/// default) output budget. Errs toward over-counting so we protect the limit.
pub fn estimate_request_tokens(req: &ChatRequest) -> u32 {
    let mut input_chars = 0usize;
    for m in &req.messages {
        input_chars += m.role.len();
        input_chars += content_len(&m.content);
        if let Some(tc) = &m.tool_calls {
            input_chars += tc.to_string().len();
        }
    }
    let input_tokens = (input_chars / 4) as u64;
    let output_tokens = req
        .config
        .as_ref()
        .and_then(|c| c.max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS_ESTIMATE);
    (input_tokens + output_tokens).clamp(1, u32::MAX as u64) as u32
}

/// Character length of a message `content` field, which may be a plain string
/// or an array of content blocks.
fn content_len(content: &serde_json::Value) -> usize {
    match content {
        serde_json::Value::String(s) => s.len(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|it| {
                it.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.len())
                    // Non-text blocks (images, etc.): count serialized size.
                    .unwrap_or_else(|| it.to_string().len())
            })
            .sum(),
        serde_json::Value::Null => 0,
        other => other.to_string().len(),
    }
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
