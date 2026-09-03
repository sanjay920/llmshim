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
use std::sync::Mutex;
use std::time::Duration;

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
