//! Per-tenant rate quotas, layered on top of the global per-provider limits.
//!
//! The proxy's [`RateLimiter`](crate::proxy::ratelimit) protects each *provider*
//! globally (so the fleet never blows the provider's TPM/RPM). This adds
//! *per-tenant* fairness on top: an authenticated caller with `rpm`/`tpm` in its
//! identity is metered by a `(tenant, provider)` token bucket, so one tenant
//! can't monopolize the shared provider capacity. It's a **proactive reject**
//! (429 + `Retry-After`) — a tenant over its own sustained rate is shed, while
//! the gateway queue still absorbs bursts against the provider limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

/// A continuously-refilling token bucket (per-minute rate → burst == rate).
struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(per_minute: u32, now: Instant) -> Self {
        let cap = (per_minute as f64).max(1.0);
        Self {
            capacity: cap,
            refill_per_sec: (cap / 60.0).max(f64::MIN_POSITIVE),
            tokens: cap,
            last: now,
        }
    }

    /// Take `want` tokens if available, else report how long until they refill.
    fn take(&mut self, want: f64, now: Instant) -> Result<(), Duration> {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens + 1e-9 >= want {
            self.tokens -= want;
            Ok(())
        } else {
            let deficit = want - self.tokens;
            Err(Duration::from_secs_f64(deficit / self.refill_per_sec))
        }
    }
}

/// Per-`(tenant, provider)` RPM + TPM buckets. Buckets are created lazily from
/// the caller's identity limits, so different tenants get independent quotas.
#[derive(Default)]
pub struct TenantQuota {
    rpm: Mutex<HashMap<String, Bucket>>,
    tpm: Mutex<HashMap<String, Bucket>>,
}

impl TenantQuota {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check a request against the tenant's per-provider quota. A no-op when the
    /// identity carries no limits (open/dev mode). `Err(retry_after)` when over.
    pub fn check(
        &self,
        tenant: &str,
        provider: &str,
        rpm: Option<u32>,
        tpm: Option<u32>,
        est_tokens: u32,
    ) -> Result<(), Duration> {
        if rpm.is_none() && tpm.is_none() {
            return Ok(());
        }
        let now = Instant::now();
        let key = format!("{tenant}:{provider}");
        let mut wait: Option<Duration> = None;

        if let Some(r) = rpm {
            let mut buckets = self.rpm.lock().unwrap();
            let b = buckets
                .entry(key.clone())
                .or_insert_with(|| Bucket::new(r, now));
            if let Err(w) = b.take(1.0, now) {
                wait = Some(w);
            }
        }
        if let Some(t) = tpm {
            let mut buckets = self.tpm.lock().unwrap();
            let b = buckets.entry(key).or_insert_with(|| Bucket::new(t, now));
            if let Err(w) = b.take(est_tokens.max(1) as f64, now) {
                wait = Some(wait.map_or(w, |cur| cur.max(w)));
            }
        }

        match wait {
            Some(w) => Err(w),
            None => Ok(()),
        }
    }
}

// ===========================================================================
// Spend cap — the same per-identity idea, denominated in dollars
// ===========================================================================

/// Default spend window when an identity sets a budget but no window: one day.
pub const DEFAULT_BUDGET_WINDOW_SECS: u64 = 86_400;

/// Seconds since the epoch, shared by every instance so a fleet agrees on which
/// window it is in. A clock that cannot be read puts everyone in window zero.
fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Identifier of the tumbling window `now` falls in. Tumbling rather than
/// sliding because the fleet-wide store is one counter per window: a sliding
/// window would need the whole event history in Redis to be exact, and an
/// inexact dollar cap is worse than a slightly coarse one.
pub(crate) fn window_index(window: Duration) -> u64 {
    let secs = window.as_secs().max(1);
    epoch_secs() / secs
}

/// Time until the current window rolls over — what a rejected caller waits.
fn window_reset(window: Duration) -> Duration {
    let secs = window.as_secs().max(1);
    Duration::from_secs(secs - (epoch_secs() % secs))
}

/// Where accrued spend is kept.
///
/// The in-memory store governs one instance. The Redis store shares a single
/// counter per `(tenant, window)` so a `$100/day` cap means one hundred dollars
/// across the fleet rather than one hundred per replica.
#[async_trait::async_trait]
pub trait SpendStore: Send + Sync {
    /// USD already charged to `tenant` in the window containing now.
    async fn spent(&self, tenant: &str, window: Duration) -> f64;
    /// Add `usd` to that same window.
    async fn record(&self, tenant: &str, window: Duration, usd: f64);
}

/// Per-instance spend ledger. Entries for elapsed windows are dropped on write.
#[derive(Default)]
pub struct InMemorySpend {
    windows: Mutex<HashMap<(String, u64), f64>>,
}

#[async_trait::async_trait]
impl SpendStore for InMemorySpend {
    async fn spent(&self, tenant: &str, window: Duration) -> f64 {
        let key = (tenant.to_string(), window_index(window));
        *self.windows.lock().unwrap().get(&key).unwrap_or(&0.0)
    }

    async fn record(&self, tenant: &str, window: Duration, usd: f64) {
        let current = window_index(window);
        let mut windows = self.windows.lock().unwrap();
        windows.retain(|(_, index), _| *index >= current);
        *windows.entry((tenant.to_string(), current)).or_insert(0.0) += usd;
    }
}

/// A dollar budget enforced per identity, alongside the RPM/TPM buckets.
///
/// Cost is only knowable *after* a response, so this is check-before-dispatch
/// and charge-after: a caller is admitted while it is under budget and rejected
/// on the first request after it crosses. One in-flight request can therefore
/// overshoot the cap; a hard pre-authorization would need a cost estimate, and
/// an estimate that is wrong in the operator's favour is its own hazard.
pub struct SpendCap {
    store: Arc<dyn SpendStore>,
}

impl Default for SpendCap {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl SpendCap {
    /// A cap governing this instance only.
    pub fn in_memory() -> Self {
        Self {
            store: Arc::new(InMemorySpend::default()),
        }
    }

    /// A cap backed by a shared store — the Redis-coordinated fleet path.
    pub fn with_store(store: Arc<dyn SpendStore>) -> Self {
        Self { store }
    }

    fn window(identity: &crate::gateway::auth::Identity) -> Duration {
        Duration::from_secs(
            identity
                .budget_window_secs
                .unwrap_or(DEFAULT_BUDGET_WINDOW_SECS)
                .max(1),
        )
    }

    /// A no-op when the identity carries no budget. `Err(retry_after)` once the
    /// window's spend has reached the cap, where the wait is the window reset.
    pub async fn check(&self, identity: &crate::gateway::auth::Identity) -> Result<(), Duration> {
        // `budget_usd: 0` freezes the key rather than unlimiting it: an
        // operator typing zero means "spend nothing", and the opposite reading
        // is the expensive one to be wrong about.
        let Some(budget) = identity.budget_usd else {
            return Ok(());
        };
        let window = Self::window(identity);
        if self.store.spent(&identity.tenant, window).await >= budget {
            return Err(window_reset(window));
        }
        Ok(())
    }

    /// Charge a completed response.
    ///
    /// `None` means the catalog could not price the model. That spend is
    /// **not** recorded — charging zero would quietly let an unpriced model run
    /// forever under a budget. Operators who need a hard cap must ensure their
    /// models are priced; `cost_usd: null` in the response is the signal.
    pub async fn record(&self, identity: &crate::gateway::auth::Identity, usd: Option<f64>) {
        let Some(usd) = usd.filter(|u| *u > 0.0) else {
            return;
        };
        if identity.budget_usd.is_none() {
            return;
        }
        self.store
            .record(&identity.tenant, Self::window(identity), usd)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn no_limits_is_a_noop() {
        let q = TenantQuota::new();
        for _ in 0..1000 {
            assert!(q.check("t", "openai", None, None, 500).is_ok());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rpm_bucket_sheds_then_refills() {
        let q = TenantQuota::new();
        // rpm=2 → burst of 2, then refill 1 / 30s.
        assert!(q.check("acme", "openai", Some(2), None, 1).is_ok());
        assert!(q.check("acme", "openai", Some(2), None, 1).is_ok());
        assert!(
            q.check("acme", "openai", Some(2), None, 1).is_err(),
            "3rd over burst"
        );

        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(
            q.check("acme", "openai", Some(2), None, 1).is_ok(),
            "refilled after 30s"
        );
    }

    fn capped(tenant: &str, budget: f64) -> crate::gateway::auth::Identity {
        crate::gateway::auth::Identity {
            tenant: tenant.into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: Some(budget),
            budget_window_secs: Some(3600),
        }
    }

    #[tokio::test]
    async fn a_budget_trips_once_the_window_is_spent() {
        let cap = SpendCap::in_memory();
        let acme = capped("acme", 1.0);

        assert!(cap.check(&acme).await.is_ok(), "under budget admits");
        cap.record(&acme, Some(0.75)).await;
        assert!(cap.check(&acme).await.is_ok(), "still under budget");
        cap.record(&acme, Some(0.30)).await;

        let retry = cap.check(&acme).await.expect_err("over budget must reject");
        assert!(
            retry > Duration::ZERO,
            "rejection must say when to come back"
        );

        // Another tenant's ledger is its own.
        assert!(cap.check(&capped("other", 1.0)).await.is_ok());
    }

    #[tokio::test]
    async fn a_zero_budget_freezes_the_key() {
        let cap = SpendCap::in_memory();
        let frozen = capped("frozen", 0.0);
        assert!(
            cap.check(&frozen).await.is_err(),
            "budget_usd: 0 must mean spend nothing, not spend anything"
        );
    }

    #[tokio::test]
    async fn no_budget_and_unpriced_responses_are_never_charged() {
        let cap = SpendCap::in_memory();
        let uncapped = crate::gateway::auth::Identity {
            tenant: "free".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
        };
        for _ in 0..100 {
            cap.record(&uncapped, Some(1_000.0)).await;
            assert!(cap.check(&uncapped).await.is_ok());
        }

        // An unpriceable response cannot be charged; it must not read as free
        // spend that silently accrues to zero either.
        let acme = capped("acme", 1.0);
        cap.record(&acme, None).await;
        assert_eq!(
            cap.store.spent("acme", Duration::from_secs(3600)).await,
            0.0
        );
        assert!(cap.check(&acme).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn tenants_are_independent() {
        let q = TenantQuota::new();
        assert!(q.check("a", "openai", Some(1), None, 1).is_ok());
        assert!(
            q.check("a", "openai", Some(1), None, 1).is_err(),
            "tenant a exhausted"
        );
        // A different tenant has its own bucket.
        assert!(q.check("b", "openai", Some(1), None, 1).is_ok());
    }
}
