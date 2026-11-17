//! The pure state behind bounded admission during a Redis outage (design note
//! 0027, slice 4a). Nothing here touches Redis or the clock: the caller passes
//! the time in, so the rules are testable. Slice 4b wires it into the gateway.
#![allow(dead_code)]

use std::collections::VecDeque;

use sha2::Digest;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::gateway::lease::{Lease, LeaseError, LeaseLedger, LocalLease};

/// Redis failures within [`TRIP_WINDOW_MS`] that put a replica into outage
/// state, and the window they are counted in. Fixed, not configurable, in the
/// first build (note 0027).
pub(crate) const TRIP_FAILURES: usize = 3;
pub(crate) const TRIP_WINDOW_MS: u64 = 10_000;
/// A lease is not spent this close to its window's end by the replica's own
/// clock: the window is chosen by Redis `TIME`, so the margin covers skew.
pub(crate) const WINDOW_MARGIN_MS: u64 = 30_000;

/// Whether a replica is on the fleet path or the local one. It enters outage
/// state after repeated failures and leaves only after Redis has answered for
/// `recover_after_ms` *and* recovery has finished, so a flapping or slow Redis
/// keeps the replica on the local path rather than switching per request.
#[derive(Debug)]
pub(crate) struct OutageState {
    recover_after_ms: u64,
    failures: VecDeque<u64>,
    in_outage: bool,
    healthy_since: Option<u64>,
    last_ok: Option<u64>,
}

impl OutageState {
    pub(crate) fn new(recover_after_ms: u64) -> Self {
        Self {
            recover_after_ms,
            failures: VecDeque::new(),
            in_outage: false,
            healthy_since: None,
            last_ok: None,
        }
    }

    pub(crate) fn in_outage(&self) -> bool {
        self.in_outage
    }

    /// Report one Redis operation's result (a probe, a fleet admission, a lease
    /// renewal). A deadline overrun is a failure.
    pub(crate) fn observe(&mut self, now_ms: u64, ok: bool) {
        if ok {
            self.last_ok = Some(now_ms);
            if self.in_outage && self.healthy_since.is_none() {
                self.healthy_since = Some(now_ms);
            }
            return;
        }
        self.healthy_since = None;
        self.failures.push_back(now_ms);
        while self.failures.len() > TRIP_FAILURES {
            self.failures.pop_front();
        }
        while self
            .failures
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) > TRIP_WINDOW_MS)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= TRIP_FAILURES {
            self.in_outage = true;
        }
    }

    /// Redis has answered, recently and without a failure, for `recover_after_ms`.
    pub(crate) fn steady(&self, now_ms: u64) -> bool {
        let recent = self
            .last_ok
            .is_some_and(|at| now_ms.saturating_sub(at) <= TRIP_WINDOW_MS);
        self.in_outage
            && recent
            && self
                .healthy_since
                .is_some_and(|since| now_ms.saturating_sub(since) >= self.recover_after_ms)
    }

    /// Try to return to the fleet path. True only when the replica was in
    /// outage state, Redis has answered without a failure for `recover_after_ms`
    /// and recovery (draw-down and release) has completed.
    pub(crate) fn try_resume(&mut self, now_ms: u64, recovery_complete: bool) -> bool {
        if self.steady(now_ms) && recovery_complete {
            self.in_outage = false;
            self.healthy_since = None;
            self.failures.clear();
            return true;
        }
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalRefusal {
    /// Within [`WINDOW_MARGIN_MS`] of the window's end (or past it).
    WindowMargin,
    /// No lease is held for this tenant, the replica is not in outage state, or
    /// recovery has begun.
    NoLease,
}

/// Where the local ledger says how much a lease has spent. The quote bound is
/// the local ledger's: each attempt (every retry, repair and fallback) reserves
/// its own quote against a budget equal to the lease before it is sent
/// (`TrustedPolicyScope::outage_view`), so the quotes admitted never exceed the
/// lease. This is only what recovery charges.
#[async_trait::async_trait]
pub(crate) trait SpendSource: Send + Sync {
    /// Nanos spent under the lease's local key, or `None` when it cannot be read.
    async fn spent_nanos(
        &self,
        tenant_key: &str,
        window_secs: u64,
        window_index: u64,
    ) -> Option<u64>;
}

/// What a request admitted locally may draw on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutageAdmission {
    pub(crate) held_nanos: u64,
    pub(crate) window_index: u64,
}

/// Held while a locally admitted request runs, so recovery can wait for it.
pub(crate) struct InFlight(Option<std::sync::Arc<OutageManager>>);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(manager) = self.0.take() {
            let mut inner = manager.inner.lock().unwrap();
            inner.in_flight = inner.in_flight.saturating_sub(1);
        }
    }
}

/// A (tenant, window length) a replica serves budgeted keys of.
type TenantWindow = (String, u64);

/// How long recovery waits for in-flight local requests before drawing what the
/// local ledger holds.
const RECOVERY_DRAIN_MS: u64 = 60_000;

/// One recovery pass draws one record per lease; it gets longer than a single
/// call, and a pass cut short resumes on the next tick.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(60);

/// A deadline for each Redis call the manager makes, so a Redis that is slow
/// rather than gone still counts as a failure.
const CALL_DEADLINE: Duration = Duration::from_secs(2);

struct Tracked {
    limit_nanos: u64,
    lease: Option<Lease>,
    last_renewed_ms: Option<u64>,
}

/// A lease as the outage began, held in memory.
#[derive(Clone, Debug)]
struct Account {
    tenant_key: String,
    window_secs: u64,
    window_index: u64,
    window_end_ms: u64,
    held_nanos: u64,
}

struct Inner {
    state: OutageState,
    tracked: HashMap<TenantWindow, Tracked>,
    /// Filled when the replica enters outage state, from the last lease it held.
    accounts: HashMap<TenantWindow, Account>,
    /// Local requests running now.
    in_flight: usize,
    /// Recovery has begun: no more local admission, even if Redis drops again,
    /// because the leases may already be released.
    recovering_since_ms: Option<u64>,
    /// Bumped on each resume: the next lease is a new one, because a released
    /// lease leaves a tombstone for its owner id in that window.
    epoch: u32,
}

/// Keeps leases while Redis is healthy, notices an outage, holds the leases,
/// and settles them on recovery (design note 0027).
pub(crate) struct OutageManager {
    ledger: LeaseLedger,
    spend: std::sync::Arc<dyn SpendSource>,
    replica: String,
    fraction: f64,
    renew_every_ms: u64,
    inner: Mutex<Inner>,
}

impl OutageManager {
    pub(crate) fn new(
        ledger: LeaseLedger,
        spend: std::sync::Arc<dyn SpendSource>,
        replica: String,
        fraction: f64,
        recover_after_ms: u64,
    ) -> Self {
        Self {
            ledger,
            spend,
            replica: replica.replace(':', "_"),
            fraction,
            renew_every_ms: 10_000,
            inner: Mutex::new(Inner {
                state: OutageState::new(recover_after_ms),
                tracked: HashMap::new(),
                accounts: HashMap::new(),
                in_flight: 0,
                recovering_since_ms: None,
                epoch: 0,
            }),
        }
    }

    pub(crate) fn in_outage(&self) -> bool {
        self.inner.lock().unwrap().state.in_outage()
    }

    /// Note that this replica serves a budgeted key of the tenant. The lease is
    /// taken on the next tick, not here, so the request path never waits on it.
    pub(crate) fn register(&self, tenant_key: &str, window_secs: u64, limit_nanos: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .tracked
            .entry((tenant_key.to_string(), window_secs))
            .and_modify(|t| t.limit_nanos = limit_nanos)
            .or_insert(Tracked {
                limit_nanos,
                lease: None,
                last_renewed_ms: None,
            });
    }

    /// Stop leasing for tenants not in `keep` (a reload removed them).
    pub(crate) fn retain_tenants(&self, keep: impl Fn(&str) -> bool) {
        self.inner
            .lock()
            .unwrap()
            .tracked
            .retain(|(tenant, _), _| keep(tenant));
    }

    /// Admit a request locally against the tenant's lease: only in outage
    /// state, before recovery has begun, with a lease held when the outage
    /// began, and not within the window margin. The guard must live as long as
    /// the request runs.
    pub(crate) fn admit(
        self: &std::sync::Arc<Self>,
        now_ms: u64,
        tenant_key: &str,
        window_secs: u64,
    ) -> Result<(OutageAdmission, InFlight), LocalRefusal> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.state.in_outage() || inner.recovering_since_ms.is_some() {
            return Err(LocalRefusal::NoLease);
        }
        let account = inner
            .accounts
            .get(&(tenant_key.to_string(), window_secs))
            .ok_or(LocalRefusal::NoLease)?;
        if now_ms.saturating_add(WINDOW_MARGIN_MS) >= account.window_end_ms {
            return Err(LocalRefusal::WindowMargin);
        }
        let admission = OutageAdmission {
            held_nanos: account.held_nanos,
            window_index: account.window_index,
        };
        inner.in_flight += 1;
        Ok((admission, InFlight(Some(self.clone()))))
    }

    /// The lease this replica holds, summed over tenants, for status (never a
    /// name). Zero once recovery has begun.
    pub(crate) fn held_nanos(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        if inner.state.in_outage() {
            if inner.recovering_since_ms.is_some() {
                return 0;
            }
            inner.accounts.values().map(|a| a.held_nanos).sum()
        } else {
            inner
                .tracked
                .values()
                .filter_map(|t| t.lease.map(|l| l.held_nanos))
                .sum()
        }
    }

    async fn deadline<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::time::timeout(CALL_DEADLINE, future).await.ok()
    }

    /// One pass: probe, renew, and (in outage state) settle and resume. The
    /// caller runs it every couple of seconds with the epoch time in ms.
    pub(crate) async fn tick(&self, now_ms: u64) {
        if self.in_outage() {
            self.tick_outage(now_ms).await;
        } else {
            self.tick_healthy(now_ms).await;
        }
    }

    fn owner(&self) -> String {
        format!("{}.{}", self.replica, self.inner.lock().unwrap().epoch)
    }

    fn observe(&self, now_ms: u64, ok: bool) {
        let was = {
            let mut inner = self.inner.lock().unwrap();
            let was = inner.state.in_outage();
            inner.state.observe(now_ms, ok);
            was
        };
        self.enter_outage_if_tripped();
        if !was && self.in_outage() {
            eprintln!("outage admission: Redis unreachable, serving budgeted keys from leases");
        }
    }

    async fn tick_healthy(&self, now_ms: u64) {
        let due: Vec<(TenantWindow, u64)> = {
            let inner = self.inner.lock().unwrap();
            inner
                .tracked
                .iter()
                .filter(|(_, t)| {
                    t.last_renewed_ms
                        .is_none_or(|at| now_ms.saturating_sub(at) >= self.renew_every_ms)
                })
                .map(|(k, t)| (k.clone(), t.limit_nanos))
                .collect()
        };
        if due.is_empty() {
            let ok = Self::deadline(self.ledger.ping()).await.unwrap_or(false);
            self.observe(now_ms, ok);
            return;
        }
        for ((tenant, window), limit) in due {
            let owner = self.owner();
            let result =
                Self::deadline(
                    self.ledger
                        .top_up(&tenant, &owner, window, limit, self.fraction),
                )
                .await;
            let mut inner = self.inner.lock().unwrap();
            match result {
                Some(Ok(lease)) => {
                    if let Some(t) = inner.tracked.get_mut(&(tenant, window)) {
                        t.lease = Some(lease);
                        t.last_renewed_ms = Some(now_ms);
                    }
                    inner.state.observe(now_ms, true);
                }
                // Not a Redis failure: the fleet is refusing, or the call was
                // malformed. Keep the old lease; it is still a valid reservation.
                Some(Err(
                    LeaseError::Refused
                    | LeaseError::Closing
                    | LeaseError::Missing
                    | LeaseError::Invalid,
                )) => {
                    inner.state.observe(now_ms, true);
                }
                Some(Err(LeaseError::Unavailable)) | None => inner.state.observe(now_ms, false),
            }
        }
        self.enter_outage_if_tripped();
    }

    /// On the transition into outage state, turn each lease held into an
    /// account. A tenant with no lease has none and is refused.
    fn enter_outage_if_tripped(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.state.in_outage() || !inner.accounts.is_empty() {
            return;
        }
        let accounts: HashMap<TenantWindow, Account> = inner
            .tracked
            .iter()
            .filter_map(|((tenant, window), t)| {
                let lease = t.lease?;
                Some((
                    (tenant.clone(), *window),
                    Account {
                        tenant_key: tenant.clone(),
                        window_secs: *window,
                        window_index: lease.window_index,
                        window_end_ms: lease.window_end_ms,
                        held_nanos: lease.held_nanos,
                    },
                ))
            })
            .collect();
        inner.accounts = accounts;
        // The leases are now spent from memory; a renewal after recovery starts fresh.
        for t in inner.tracked.values_mut() {
            t.lease = None;
            t.last_renewed_ms = None;
        }
    }

    async fn tick_outage(&self, now_ms: u64) {
        let ok = Self::deadline(self.ledger.ping()).await.unwrap_or(false);
        self.observe(now_ms, ok);
        if !ok {
            return;
        }
        let accounts: Vec<Account> = {
            let mut inner = self.inner.lock().unwrap();
            // Settle only after Redis has answered steadily, so a flapping
            // Redis keeps local admission on its leases.
            if inner.recovering_since_ms.is_none() {
                if !inner.state.steady(now_ms) {
                    return;
                }
                inner.recovering_since_ms = Some(now_ms);
            }
            let since = inner.recovering_since_ms.unwrap_or(now_ms);
            // Running requests get a bound to finish; past it the lease is
            // drawn at what the local ledger holds, as a fleet attempt that
            // times out is settled at its reservation.
            if inner.in_flight > 0 && now_ms.saturating_sub(since) < RECOVERY_DRAIN_MS {
                return;
            }
            inner.accounts.values().cloned().collect()
        };
        let owner = self.owner();
        let mut records = Vec::with_capacity(accounts.len());
        for account in &accounts {
            let Some(spent) = self
                .spend
                .spent_nanos(
                    &account.tenant_key,
                    account.window_secs,
                    account.window_index,
                )
                .await
            else {
                self.observe(now_ms, false);
                return;
            };
            // One record per lease, with an id the same lease always gets, so a
            // pass that stops part-way and runs again draws it once.
            let digest = sha2::Sha256::digest(
                format!("{owner}:{}:{}", account.tenant_key, account.window_index).as_bytes(),
            );
            let mut bytes = [0_u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            let id = uuid::Uuid::from_bytes(bytes);
            records.push(LocalLease {
                tenant_key: account.tenant_key.clone(),
                window_secs: account.window_secs,
                window_index: account.window_index,
                attempts: vec![(id, spent)],
            });
        }
        let complete = if records.is_empty() {
            true
        } else {
            tokio::time::timeout(RECOVERY_DEADLINE, self.ledger.recover(&owner, &records))
                .await
                .is_ok_and(|report| report.complete())
        };
        if !complete {
            // Redis answers a ping but not the settlement: not healthy.
            self.observe(now_ms, false);
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.state.try_resume(now_ms, true) {
            inner.accounts.clear();
            inner.recovering_since_ms = None;
            inner.epoch += 1;
            eprintln!("outage admission: Redis is back, leases settled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A spend source the test sets.
    #[derive(Default)]
    struct Spent(std::sync::atomic::AtomicU64);

    #[async_trait::async_trait]
    impl SpendSource for Spent {
        async fn spent_nanos(&self, _: &str, _: u64, _: u64) -> Option<u64> {
            Some(self.0.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    fn manager(
        via: &str,
        index: &str,
        spent: &Arc<Spent>,
        recover_after_ms: u64,
    ) -> Arc<OutageManager> {
        let ledger = LeaseLedger::new(via, 60, index.into(), 1000).unwrap();
        Arc::new(OutageManager::new(
            ledger,
            spent.clone(),
            "r:1".into(),
            0.1,
            recover_after_ms,
        ))
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn repeated_failures_trip_the_outage_state_and_scattered_ones_do_not() {
        let mut s = OutageState::new(30_000);
        s.observe(0, false);
        s.observe(20_000, false);
        s.observe(40_000, false);
        assert!(!s.in_outage(), "failures outside the window do not add up");
        let mut s = OutageState::new(30_000);
        for t in [0, 1_000, 2_000] {
            s.observe(t, false);
        }
        assert!(s.in_outage());
    }

    #[test]
    fn leaving_needs_steady_health_and_finished_recovery() {
        let mut s = OutageState::new(30_000);
        for t in [0, 1, 2] {
            s.observe(t, false);
        }
        s.observe(1_000, true);
        assert!(!s.try_resume(10_000, true), "not yet steady");
        s.observe(35_000, true);
        assert!(!s.try_resume(40_000, false), "recovery unfinished");
        assert!(s.try_resume(40_000, true));
        assert!(!s.in_outage());
    }

    #[test]
    fn a_flapping_redis_restarts_the_clock_and_never_resumes() {
        let mut s = OutageState::new(30_000);
        for t in [0, 1, 2] {
            s.observe(t, false);
        }
        let mut now = 1_000;
        for _ in 0..10 {
            s.observe(now, true);
            now += 20_000;
            s.observe(now, false);
            assert!(!s.try_resume(now, true));
            now += 1_000;
        }
        assert!(s.in_outage());
    }

    #[test]
    fn silence_is_not_health() {
        let mut s = OutageState::new(30_000);
        for t in [0, 1, 2] {
            s.observe(t, false);
        }
        s.observe(1_000, true);
        assert!(!s.try_resume(60_000, true), "one answer long ago");
    }

    /// A TCP forwarder to Redis that a test can cut and restore.
    struct Proxy {
        address: std::net::SocketAddr,
        state: tokio::sync::watch::Sender<(bool, u64)>,
    }

    impl Proxy {
        async fn start(target: &str) -> Self {
            let authority = target
                .trim_start_matches("redis://")
                .split('/')
                .next()
                .unwrap()
                .to_owned();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (state, watch) = tokio::sync::watch::channel((false, 0_u64));
            tokio::spawn(async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    let (cut, generation) = *watch.borrow();
                    if cut {
                        continue;
                    }
                    let mut watch = watch.clone();
                    let authority = authority.clone();
                    tokio::spawn(async move {
                        let Ok(mut upstream) = tokio::net::TcpStream::connect(&authority).await
                        else {
                            return;
                        };
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                            _ = watch.wait_for(|(_, now)| *now != generation) => {}
                        }
                    });
                }
            });
            Self { address, state }
        }
        fn cut(&self) {
            self.state.send_modify(|(cut, g)| {
                *cut = true;
                *g += 1;
            });
        }
        fn restore(&self) {
            self.state.send_modify(|(cut, _)| *cut = false);
        }
    }

    async fn counter_at(url: &str, tenant: &str, window: u64, index: u64) -> u64 {
        let ledger = LeaseLedger::new(url, 60, "idx".into(), 1000).unwrap();
        ledger.counter(tenant, window, index).await
    }

    async fn into_outage(manager: &Arc<OutageManager>, proxy: &Proxy) {
        proxy.cut();
        for t in [2_000, 3_000, 4_000] {
            manager.tick(t).await;
        }
        assert!(manager.in_outage());
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn an_outage_is_served_from_the_lease_and_settled_on_recovery() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let spent = Arc::new(Spent::default());
        let index = format!("llmshim:gw:test-index:{}", uuid::Uuid::new_v4().simple());
        let manager = manager(&via, &index, &spent, 100);
        let tenant = format!("outage-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);

        // Healthy: a lease is taken, and nothing is admitted locally.
        manager.tick(1_000).await;
        assert_eq!(manager.held_nanos(), 100_000_000);
        assert!(matches!(
            manager.admit(now_ms(), &tenant, 3600),
            Err(LocalRefusal::NoLease)
        ));
        let window = now_ms() / 1000 / 3600;
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 100_000_000);

        // Redis goes away: three failed probes trip the outage state.
        into_outage(&manager, &proxy).await;

        // Admitted against the lease; a tenant with no lease is refused.
        let (admission, guard) = manager.admit(now_ms(), &tenant, 3600).unwrap();
        assert_eq!(admission.held_nanos, 100_000_000);
        assert!(matches!(
            manager.admit(now_ms(), "other", 3600),
            Err(LocalRefusal::NoLease)
        ));
        // The local ledger says 115M was spent: 15M over the lease.
        spent
            .0
            .store(115_000_000, std::sync::atomic::Ordering::SeqCst);

        // Redis returns; a request is still running, so recovery waits.
        proxy.restore();
        for t in [5_000, 5_050, 5_300] {
            manager.tick(t).await;
        }
        assert!(manager.in_outage());
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 100_000_000);
        // Nothing new is admitted once recovery has begun.
        assert!(matches!(
            manager.admit(now_ms(), &tenant, 3600),
            Err(LocalRefusal::NoLease)
        ));
        drop(guard);
        manager.tick(5_400).await;
        assert!(!manager.in_outage());
        manager.tick(5_500).await;
        // 115M spent (100M of lease plus 15M of excess), nothing held back from the
        // old lease; the replica took a new one (10 % of what remains, 885M).
        let fresh = manager.held_nanos();
        assert_eq!(fresh, 88_500_000);
        assert_eq!(
            counter_at(&url, &tenant, 3600, window).await,
            115_000_000 + fresh
        );
        manager.tick(20_000).await;
        assert_eq!(manager.held_nanos(), fresh, "renewal holds the target");
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_flapping_redis_keeps_local_admission_on_the_lease_and_settles_nothing() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let spent = Arc::new(Spent::default());
        let manager = manager(&via, "idx-flap", &spent, 30_000);
        let tenant = format!("flap-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);
        manager.tick(1_000).await;
        into_outage(&manager, &proxy).await;
        let mut t = 5_000;
        let mut guards = Vec::new();
        for _ in 0..4 {
            proxy.restore();
            manager.tick(t).await;
            proxy.cut();
            manager.tick(t + 1_000).await;
            t += 10_000;
            guards.push(manager.admit(now_ms(), &tenant, 3600).unwrap());
        }
        assert!(manager.in_outage());
        let window = now_ms() / 1000 / 3600;
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 100_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn nothing_is_admitted_inside_the_window_margin() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let spent = Arc::new(Spent::default());
        let manager = manager(&via, "idx-margin", &spent, 100);
        let tenant = format!("margin-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);
        manager.tick(1_000).await;
        into_outage(&manager, &proxy).await;
        let window_end = (now_ms() / 1000 / 3600 + 1) * 3600 * 1000;
        assert!(manager
            .admit(window_end - WINDOW_MARGIN_MS - 1, &tenant, 3600)
            .is_ok());
        assert_eq!(
            manager
                .admit(window_end - WINDOW_MARGIN_MS, &tenant, 3600)
                .err(),
            Some(LocalRefusal::WindowMargin)
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_second_outage_in_the_same_window_has_its_own_lease() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let spent = Arc::new(Spent::default());
        let manager = manager(&via, "idx-second", &spent, 100);
        let tenant = format!("second-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);
        manager.tick(1_000).await;
        into_outage(&manager, &proxy).await;
        proxy.restore();
        for t in [5_000, 5_300] {
            manager.tick(t).await;
        }
        manager.tick(5_400).await;
        assert!(!manager.in_outage());
        proxy.cut();
        for t in [30_000, 31_000, 32_000] {
            manager.tick(t).await;
        }
        assert!(manager.in_outage());
        assert!(manager.admit(now_ms(), &tenant, 3600).is_ok());
    }
}
