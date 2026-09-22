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

const ZERO_QUOTA_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A continuously-refilling token bucket (per-minute rate → burst == rate).
struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(per_minute: u32, now: Instant) -> Self {
        let capacity = per_minute as f64;
        Self {
            capacity,
            refill_per_sec: if capacity > 0.0 { capacity / 60.0 } else { 0.0 },
            last: now,
            tokens: capacity,
        }
    }

    /// Take `want` tokens if available, else report how long until they refill.
    fn take(&mut self, want: f64, now: Instant) -> Result<(), Duration> {
        if self.capacity == 0.0 {
            return Err(ZERO_QUOTA_RETRY_AFTER);
        }
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

    /// Check a request against the tenant's per-provider quota. An omitted limit
    /// is unlimited; an explicit zero denies every request. `Err(retry_after)`
    /// is returned when a request is denied.
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
        // Check before creating or charging either bucket. A zero tenant limit
        // is a permanent policy decision, and must not consume another bucket.
        if rpm == Some(0) || tpm == Some(0) {
            return Err(ZERO_QUOTA_RETRY_AFTER);
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

/// Why a budgeted request was refused.
///
/// The two are kept apart because the correct answer to the caller differs.
/// `Exhausted` is a wait — the window resets and the same request succeeds.
/// `Unpriceable` is a deployment fact: the catalog has no price for this target,
/// so the cap cannot be enforced against it and no amount of retrying changes
/// that. Collapsing them into one 429 would tell an operator to wait for a
/// condition that never clears.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetRefusal {
    /// The window's spend has reached the cap. Carries the time until reset.
    Exhausted(Duration),
    /// The catalog cannot price this target, and the identity has not opted in
    /// to running unpriced under a cap.
    Unpriceable,
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

    /// A no-op when the identity carries no budget.
    ///
    /// Two ways a budgeted request is refused, and they are not the same event:
    /// [`BudgetRefusal::Exhausted`] is temporary and clears at the window reset;
    /// [`BudgetRefusal::Unpriceable`] never clears on its own, because the
    /// catalog has no price for the target and retrying changes nothing.
    pub async fn check(
        &self,
        identity: &crate::gateway::auth::Identity,
        provider: &str,
        model: &str,
    ) -> Result<(), BudgetRefusal> {
        // `budget_usd: 0` freezes the key rather than unlimiting it: an
        // operator typing zero means "spend nothing", and the opposite reading
        // is the expensive one to be wrong about.
        let Some(budget) = identity.budget_usd else {
            return Ok(());
        };

        // A response the catalog cannot price is spend the ledger never sees.
        // Allowing it under a cap does not merely lose one charge — it makes the
        // cap stop binding for every later request too, silently. Refusing is the
        // only outcome that keeps "a budget is set" and "the budget is enforced"
        // the same statement.
        if !crate::cost::is_priceable(provider, model) && !identity.budget_allow_unpriced {
            return Err(BudgetRefusal::Unpriceable);
        }

        let window = Self::window(identity);
        if self.store.spent(&identity.tenant, window).await >= budget {
            return Err(BudgetRefusal::Exhausted(window_reset(window)));
        }
        Ok(())
    }

    /// Whether this request is about to run unpriced under a cap.
    ///
    /// True only when the operator opted in. Callers report it so an accepted
    /// risk stays measurable instead of becoming an assumption.
    #[must_use]
    pub fn is_unpriced_under_cap(
        identity: &crate::gateway::auth::Identity,
        provider: &str,
        model: &str,
    ) -> bool {
        identity.budget_usd.is_some() && !crate::cost::is_priceable(provider, model)
    }

    /// Charge a completed response.
    ///
    /// `None` means the catalog could not price the model, and that spend is
    /// **not** recorded — charging zero would quietly let an unpriced model run
    /// forever under a budget.
    ///
    /// This used to be the whole story, and it was a hole: the cap silently
    /// stopped binding and the only signal was a `null` in a response body an
    /// operator had to notice. [`SpendCap::check`] now refuses unpriceable
    /// targets under a budget before they run, so reaching here with `None`
    /// means the operator set `budget_allow_unpriced` and accepted it.
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
            budget_allow_unpriced: false,
        }
    }

    /// A target the catalog prices. Asserted rather than assumed: if the catalog
    /// stops pricing it, these tests must fail loudly rather than quietly start
    /// exercising the unpriceable path instead of the budget path.
    const PRICED: (&str, &str) = ("anthropic", "claude-sonnet-4-6");

    /// A model the catalog does not price. Deliberately implausible so it cannot
    /// start being priced by a catalog refresh and quietly neuter these tests.
    const UNPRICED: &str = "definitely-not-a-model-xyz";

    #[tokio::test]
    async fn a_budget_trips_once_the_window_is_spent() {
        let cap = SpendCap::in_memory();
        let acme = capped("acme", 1.0);

        assert!(
            crate::cost::is_priceable(PRICED.0, PRICED.1),
            "test fixture must be priced or this tests the wrong path"
        );

        let (p, m) = PRICED;
        assert!(cap.check(&acme, p, m).await.is_ok(), "under budget admits");
        cap.record(&acme, Some(0.75)).await;
        assert!(cap.check(&acme, p, m).await.is_ok(), "still under budget");
        cap.record(&acme, Some(0.30)).await;

        match cap.check(&acme, p, m).await {
            Err(BudgetRefusal::Exhausted(retry)) => assert!(
                retry > Duration::ZERO,
                "rejection must say when to come back"
            ),
            other => panic!("over budget must reject as Exhausted, got {other:?}"),
        }

        // Another tenant's ledger is its own.
        assert!(cap.check(&capped("other", 1.0), p, m).await.is_ok());
    }

    /// A target with no catalog price cannot be charged. Under a cap that is not
    /// a lost charge, it is a cap that stops binding — so it must be refused, and
    /// refused *differently* from being out of budget.
    #[tokio::test]
    async fn an_unpriceable_model_is_refused_under_a_budget() {
        let cap = SpendCap::in_memory();
        let acme = capped("acme", 100.0);
        assert!(
            !crate::cost::is_priceable("anthropic", UNPRICED),
            "fixture must be unpriced or this tests nothing"
        );

        let refusal = cap
            .check(&acme, "anthropic", UNPRICED)
            .await
            .expect_err("an unpriceable model under a cap must be refused");
        assert_eq!(
            refusal,
            BudgetRefusal::Unpriceable,
            "must not masquerade as Exhausted: retrying never clears this"
        );

        // The budget is nowhere near spent — the refusal is about priceability.
        assert!(cap.check(&acme, PRICED.0, PRICED.1).await.is_ok());
    }

    /// Opting in is allowed, because a new model can outrun the catalog. It is
    /// explicit, per-key, and defaults to off.
    #[tokio::test]
    async fn an_operator_can_opt_in_to_running_unpriced() {
        let cap = SpendCap::in_memory();
        let mut acme = capped("acme", 100.0);
        acme.budget_allow_unpriced = true;

        assert!(
            cap.check(&acme, "anthropic", UNPRICED).await.is_ok(),
            "explicit opt-in must admit"
        );
        assert!(
            SpendCap::is_unpriced_under_cap(&acme, "anthropic", UNPRICED),
            "and the caller must be able to see it, so the hole stays measurable"
        );
        assert!(
            !SpendCap::is_unpriced_under_cap(&acme, PRICED.0, PRICED.1),
            "a priced target is not a hole"
        );
    }

    /// With no cap there is nothing to enforce, so priceability is irrelevant.
    /// Refusing here would break every unbudgeted key the moment a model is new.
    #[tokio::test]
    async fn without_a_budget_an_unpriceable_model_is_fine() {
        let cap = SpendCap::in_memory();
        let free = crate::gateway::auth::Identity {
            tenant: "free".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
        };
        assert!(cap.check(&free, "anthropic", UNPRICED).await.is_ok());
        assert!(!SpendCap::is_unpriced_under_cap(
            &free,
            "anthropic",
            UNPRICED
        ));
    }

    #[tokio::test]
    async fn a_zero_budget_freezes_the_key() {
        let cap = SpendCap::in_memory();
        let frozen = capped("frozen", 0.0);
        assert!(
            matches!(
                cap.check(&frozen, PRICED.0, PRICED.1).await,
                Err(BudgetRefusal::Exhausted(_))
            ),
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
            budget_allow_unpriced: false,
        };
        for _ in 0..100 {
            cap.record(&uncapped, Some(1_000.0)).await;
            assert!(cap.check(&uncapped, PRICED.0, PRICED.1).await.is_ok());
        }

        // An unpriceable response cannot be charged; it must not read as free
        // spend that silently accrues to zero either.
        let acme = capped("acme", 1.0);
        cap.record(&acme, None).await;
        assert_eq!(
            cap.store.spent("acme", Duration::from_secs(3600)).await,
            0.0
        );
        assert!(cap.check(&acme, PRICED.0, PRICED.1).await.is_ok());
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

    #[tokio::test(start_paused = true)]
    async fn zero_limits_deny_without_consuming_other_bucket() {
        let q = TenantQuota::new();

        assert!(q.check("rpm-zero", "openai", Some(0), Some(1), 1).is_err());
        assert!(q.check("rpm-zero", "openai", None, Some(1), 1).is_ok());

        assert!(q.check("tpm-zero", "openai", Some(1), Some(0), 1).is_err());
        assert!(q.check("tpm-zero", "openai", Some(1), None, 1).is_ok());
    }
}
