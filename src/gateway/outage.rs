//! The pure state behind bounded admission during a Redis outage (design note
//! 0027, slice 4a). Nothing here touches Redis or the clock: the caller passes
//! the time in, so the rules are testable. Slice 4b wires it into the gateway.
#![allow(dead_code)]

use std::collections::VecDeque;

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
    /// The quote exceeds what the lease has left.
    Exhausted,
    /// This attempt id already holds a reservation.
    Duplicate,
    /// No lease is held for this tenant, or the replica is not in outage state.
    NoLease,
}

/// One replica's in-memory account of one lease while Redis is out. Every
/// attempt (each retry, repair and fallback included) reserves its own quote
/// before it is sent, and a quote above the unspent lease is refused, so the
/// quotes admitted never exceed the lease: the bound of note 0027. It is a
/// bound on quotes; a provider can still bill above one, and recovery charges
/// the excess.
#[derive(Debug)]
pub(crate) struct LocalLeaseAccount {
    tenant_key: String,
    window_secs: u64,
    window_index: u64,
    window_end_ms: u64,
    held_nanos: u64,
    reserved_nanos: u64,
    attempts: Vec<Attempt>,
}

#[derive(Debug)]
struct Attempt {
    id: uuid::Uuid,
    quote: u64,
    /// The amount to charge: the quote until the attempt reports a final one.
    charge: u64,
    cancelled: bool,
    /// Reported final (or cancelled): no longer in flight.
    done: bool,
}

impl LocalLeaseAccount {
    pub(crate) fn new(
        tenant_key: String,
        window_secs: u64,
        window_index: u64,
        window_end_ms: u64,
        held_nanos: u64,
    ) -> Self {
        Self {
            tenant_key,
            window_secs,
            window_index,
            window_end_ms,
            held_nanos,
            reserved_nanos: 0,
            attempts: Vec::new(),
        }
    }

    pub(crate) fn unspent_nanos(&self) -> u64 {
        self.held_nanos - self.reserved_nanos
    }

    /// Reserve an attempt's quote, or refuse. `now_ms` is the replica's own
    /// clock in epoch milliseconds.
    pub(crate) fn reserve(
        &mut self,
        now_ms: u64,
        attempt: uuid::Uuid,
        quote_nanos: u64,
    ) -> Result<(), LocalRefusal> {
        if now_ms.saturating_add(WINDOW_MARGIN_MS) >= self.window_end_ms {
            return Err(LocalRefusal::WindowMargin);
        }
        if self.attempts.iter().any(|a| a.id == attempt) {
            return Err(LocalRefusal::Duplicate);
        }
        if quote_nanos > self.unspent_nanos() {
            return Err(LocalRefusal::Exhausted);
        }
        self.reserved_nanos += quote_nanos;
        self.attempts.push(Attempt {
            id: attempt,
            quote: quote_nanos,
            charge: quote_nanos,
            cancelled: false,
            done: false,
        });
        Ok(())
    }

    /// Attempts reserved and not yet finished or cancelled.
    pub(crate) fn in_flight(&self) -> usize {
        self.attempts.iter().filter(|a| !a.done).count()
    }

    /// An attempt that was never sent gives its quote back.
    pub(crate) fn cancel(&mut self, attempt: uuid::Uuid) {
        if let Some(a) = self
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt && !a.cancelled)
        {
            a.cancelled = true;
            a.done = true;
            self.reserved_nanos -= a.quote;
        }
    }

    /// Record an attempt's final amount for recovery. The reservation is kept
    /// as it was: a lease is spent once, never refilled mid-outage.
    pub(crate) fn finish(&mut self, attempt: uuid::Uuid, final_nanos: u64) {
        if let Some(a) = self
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt && !a.cancelled)
        {
            a.charge = final_nanos;
            a.done = true;
        }
    }

    /// What recovery draws: each attempt that was sent, at its final amount (its
    /// quote when it never reported one).
    pub(crate) fn to_local_lease(&self) -> LocalLease {
        LocalLease {
            tenant_key: self.tenant_key.clone(),
            window_secs: self.window_secs,
            window_index: self.window_index,
            attempts: self
                .attempts
                .iter()
                .filter(|a| !a.cancelled)
                .map(|a| (a.id, a.charge))
                .collect(),
        }
    }
}

/// A (tenant, window length) a replica serves budgeted keys of.
type TenantWindow = (String, u64);

/// A deadline for each Redis call the manager makes, so a Redis that is slow
/// rather than gone still counts as a failure.
/// How long recovery waits for in-flight local attempts before drawing them at
/// their quote.
const RECOVERY_DRAIN_MS: u64 = 60_000;

const CALL_DEADLINE: Duration = Duration::from_secs(2);

struct Tracked {
    limit_nanos: u64,
    lease: Option<Lease>,
    last_renewed_ms: Option<u64>,
}

struct Inner {
    state: OutageState,
    tracked: HashMap<TenantWindow, Tracked>,
    /// Filled when the replica enters outage state, from the last lease it held.
    accounts: HashMap<TenantWindow, LocalLeaseAccount>,
    /// Recovery has begun: no more local admission, even if Redis drops again,
    /// because the leases may already be released.
    recovering_since_ms: Option<u64>,
}

/// Keeps leases while Redis is healthy, notices an outage, holds the local
/// accounts, and settles them on recovery (design note 0027). It owns no
/// request path: slice 4c asks it to reserve and to report.
pub(crate) struct OutageManager {
    ledger: LeaseLedger,
    replica: String,
    fraction: f64,
    renew_every_ms: u64,
    inner: Mutex<Inner>,
}

impl OutageManager {
    pub(crate) fn new(
        ledger: LeaseLedger,
        replica: String,
        fraction: f64,
        recover_after_ms: u64,
    ) -> Self {
        Self {
            ledger,
            replica,
            fraction,
            renew_every_ms: 10_000,
            inner: Mutex::new(Inner {
                state: OutageState::new(recover_after_ms),
                tracked: HashMap::new(),
                accounts: HashMap::new(),
                recovering_since_ms: None,
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

    /// Reserve a local attempt's quote against the tenant's lease. Only while
    /// the replica is in outage state, and only against a lease it held when the
    /// outage began.
    pub(crate) fn reserve(
        &self,
        now_ms: u64,
        tenant_key: &str,
        window_secs: u64,
        attempt: uuid::Uuid,
        quote_nanos: u64,
    ) -> Result<(), LocalRefusal> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.state.in_outage() || inner.recovering_since_ms.is_some() {
            return Err(LocalRefusal::NoLease);
        }
        match inner
            .accounts
            .get_mut(&(tenant_key.to_string(), window_secs))
        {
            Some(account) => account.reserve(now_ms, attempt, quote_nanos),
            None => Err(LocalRefusal::NoLease),
        }
    }

    pub(crate) fn cancel(&self, tenant_key: &str, window_secs: u64, attempt: uuid::Uuid) {
        if let Some(a) = self
            .inner
            .lock()
            .unwrap()
            .accounts
            .get_mut(&(tenant_key.to_string(), window_secs))
        {
            a.cancel(attempt);
        }
    }

    pub(crate) fn finish(
        &self,
        tenant_key: &str,
        window_secs: u64,
        attempt: uuid::Uuid,
        final_nanos: u64,
    ) {
        if let Some(a) = self
            .inner
            .lock()
            .unwrap()
            .accounts
            .get_mut(&(tenant_key.to_string(), window_secs))
        {
            a.finish(attempt, final_nanos);
        }
    }

    /// The unspent lease, summed over tenants, for status (never a name).
    pub(crate) fn unspent_nanos(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        if inner.state.in_outage() {
            inner.accounts.values().map(|a| a.unspent_nanos()).sum()
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
            let result = Self::deadline(self.ledger.top_up(
                &tenant,
                &self.replica,
                window,
                limit,
                self.fraction,
            ))
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
                // Not a Redis failure: the fleet is refusing, or this lease is
                // closing. Keep the old lease; it is still a valid reservation.
                Some(Err(LeaseError::Refused | LeaseError::Closing | LeaseError::Missing)) => {
                    inner.state.observe(now_ms, true);
                }
                Some(Err(LeaseError::Unavailable)) | None => inner.state.observe(now_ms, false),
            }
        }
        self.enter_outage_if_tripped();
    }

    fn observe(&self, now_ms: u64, ok: bool) {
        self.inner.lock().unwrap().state.observe(now_ms, ok);
        self.enter_outage_if_tripped();
    }

    /// On the transition into outage state, turn each lease held into a local
    /// account. A tenant with no lease has no account and is refused.
    fn enter_outage_if_tripped(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.state.in_outage() || !inner.accounts.is_empty() {
            return;
        }
        let accounts: HashMap<TenantWindow, LocalLeaseAccount> = inner
            .tracked
            .iter()
            .filter_map(|((tenant, window), t)| {
                let lease = t.lease?;
                Some((
                    (tenant.clone(), *window),
                    LocalLeaseAccount::new(
                        tenant.clone(),
                        *window,
                        lease.window_index,
                        lease.window_end_ms,
                        lease.held_nanos,
                    ),
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
        let records: Vec<LocalLease> = {
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
            // In-flight attempts get a bound to finish; past it they are drawn
            // at their quote, as a fleet attempt that times out is.
            let waiting = inner.accounts.values().any(|a| a.in_flight() > 0)
                && now_ms.saturating_sub(since) < RECOVERY_DRAIN_MS;
            if waiting {
                return;
            }
            inner
                .accounts
                .values()
                .map(|a| a.to_local_lease())
                .collect()
        };
        let complete = if records.is_empty() {
            true
        } else {
            match Self::deadline(self.ledger.recover(&self.replica, &records)).await {
                Some(report) => report.complete(),
                None => false,
            }
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.state.try_resume(now_ms, complete) {
            inner.accounts.clear();
            inner.recovering_since_ms = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> uuid::Uuid {
        uuid::Uuid::new_v4()
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
    fn quotes_never_exceed_the_lease() {
        let mut a = LocalLeaseAccount::new("t".into(), 3600, 7, 10_000_000, 100);
        let mut admitted = 0;
        for q in [30, 30, 30, 30, 5, 5, 10] {
            if a.reserve(0, id(), q).is_ok() {
                admitted += q;
            }
        }
        assert!(admitted <= 100);
        assert_eq!(admitted, 100);
        assert_eq!(a.unspent_nanos(), 0);
        assert_eq!(a.reserve(0, id(), 1), Err(LocalRefusal::Exhausted));
    }

    #[test]
    fn nothing_is_admitted_inside_the_window_margin() {
        let end = 3_600_000;
        let mut a = LocalLeaseAccount::new("t".into(), 3600, 0, end, 100);
        assert!(a.reserve(end - WINDOW_MARGIN_MS - 1, id(), 1).is_ok());
        assert_eq!(
            a.reserve(end - WINDOW_MARGIN_MS, id(), 1),
            Err(LocalRefusal::WindowMargin)
        );
        assert_eq!(a.reserve(end + 5, id(), 1), Err(LocalRefusal::WindowMargin));
    }

    #[test]
    fn a_cancelled_attempt_gives_its_quote_back_and_is_not_drawn() {
        let mut a = LocalLeaseAccount::new("t".into(), 3600, 0, 10_000_000, 10);
        let (x, y) = (id(), id());
        a.reserve(0, x, 6).unwrap();
        assert_eq!(a.reserve(0, y, 6), Err(LocalRefusal::Exhausted));
        a.cancel(x);
        a.cancel(x);
        assert_eq!(a.unspent_nanos(), 10);
        a.reserve(0, y, 6).unwrap();
        let rec = a.to_local_lease();
        assert_eq!(rec.attempts, vec![(y, 6)]);
    }

    #[test]
    fn recovery_charges_the_final_amount_and_the_quote_when_none_was_reported() {
        let mut a = LocalLeaseAccount::new("t".into(), 3600, 3, 10_000_000, 100);
        let (x, y) = (id(), id());
        a.reserve(0, x, 40).unwrap();
        a.reserve(0, y, 40).unwrap();
        a.finish(x, 55);
        let rec = a.to_local_lease();
        assert_eq!(rec.attempts, vec![(x, 55), (y, 40)]);
        assert_eq!((rec.window_index, rec.window_secs), (3, 3600));
        // A final above the quote does not refill or enlarge the lease mid-outage.
        assert_eq!(a.unspent_nanos(), 20);
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

    #[test]
    fn an_attempt_id_reserves_once() {
        let mut a = LocalLeaseAccount::new("t".into(), 3600, 0, 10_000_000, 100);
        let x = id();
        a.reserve(0, x, 40).unwrap();
        assert_eq!(a.reserve(0, x, 40), Err(LocalRefusal::Duplicate));
        assert_eq!(a.unspent_nanos(), 60);
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

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn an_outage_is_served_from_the_lease_and_settled_on_recovery() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let index = format!("llmshim:gw:test-index:{}", uuid::Uuid::new_v4().simple());
        let ledger = LeaseLedger::new(&via, 60, index, 1000).unwrap();
        let manager = OutageManager::new(ledger, "r1".into(), 0.1, 100);
        let tenant = format!("outage-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);

        // Healthy: a lease is taken, and nothing is admitted locally.
        manager.tick(1_000).await;
        assert_eq!(manager.unspent_nanos(), 100_000_000);
        assert_eq!(
            manager.reserve(1_000, &tenant, 3600, id(), 1),
            Err(LocalRefusal::NoLease)
        );
        let now_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let window = now_epoch_ms / 1000 / 3600;
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 100_000_000);

        // Redis goes away: three failed probes trip the outage state.
        proxy.cut();
        for t in [2_000, 3_000, 4_000] {
            manager.tick(t).await;
        }
        assert!(manager.in_outage());

        // Served from the lease until it is spent; never beyond it.
        let (first, second, third) = (id(), id(), id());
        manager
            .reserve(now_epoch_ms, &tenant, 3600, first, 60_000_000)
            .unwrap();
        manager
            .reserve(now_epoch_ms, &tenant, 3600, second, 40_000_000)
            .unwrap();
        assert_eq!(
            manager.reserve(now_epoch_ms, &tenant, 3600, third, 1),
            Err(LocalRefusal::Exhausted)
        );
        // A tenant with no lease is refused.
        assert_eq!(
            manager.reserve(now_epoch_ms, "other", 3600, id(), 1),
            Err(LocalRefusal::NoLease)
        );
        manager.finish(&tenant, 3600, first, 70_000_000);
        manager.finish(&tenant, 3600, second, 45_000_000);

        // Redis returns: after steady health the leases are drawn and released.
        proxy.restore();
        for t in [5_000, 5_050, 5_300, 5_400] {
            manager.tick(t).await;
        }
        assert!(!manager.in_outage());
        // 115M spent: 100M of lease plus 15M of excess; nothing is held back.
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 115_000_000);
        // Back on the fleet path: a renewal takes a fresh lease of what remains.
        manager.tick(20_000).await;
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_flapping_redis_keeps_local_admission_on_the_lease_and_settles_nothing() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let proxy = Proxy::start(&url).await;
        let via = format!("redis://{}", proxy.address);
        let ledger = LeaseLedger::new(&via, 60, "idx-flap".into(), 1000).unwrap();
        let manager = OutageManager::new(ledger, "r1".into(), 0.1, 30_000);
        let tenant = format!("flap-{}", uuid::Uuid::new_v4().simple());
        manager.register(&tenant, 3600, 1_000_000_000);
        manager.tick(1_000).await;
        proxy.cut();
        for t in [2_000, 3_000, 4_000] {
            manager.tick(t).await;
        }
        assert!(manager.in_outage());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let mut t = 5_000;
        for _ in 0..4 {
            proxy.restore();
            manager.tick(t).await;
            proxy.cut();
            manager.tick(t + 1_000).await;
            t += 10_000;
            // Still on the lease: the first reserve of each round fits.
            manager
                .reserve(now, &tenant, 3600, id(), 10_000_000)
                .unwrap();
        }
        assert!(manager.in_outage());
        // Nothing was drawn: the lease is still whole in Redis.
        let window = now / 1000 / 3600;
        assert_eq!(counter_at(&url, &tenant, 3600, window).await, 100_000_000);
    }
}
