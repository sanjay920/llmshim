//! The pure state behind bounded admission during a Redis outage (design note
//! 0027, slice 4a). Nothing here touches Redis or the clock: the caller passes
//! the time in, so the rules are testable. Slice 4b wires it into the gateway.
#![allow(dead_code)]

use std::collections::VecDeque;

use crate::gateway::lease::LocalLease;

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
}

impl OutageState {
    pub(crate) fn new(recover_after_ms: u64) -> Self {
        Self {
            recover_after_ms,
            failures: VecDeque::new(),
            in_outage: false,
            healthy_since: None,
        }
    }

    pub(crate) fn in_outage(&self) -> bool {
        self.in_outage
    }

    /// Report one Redis operation's result (a probe, a fleet admission, a lease
    /// renewal). A deadline overrun is a failure.
    pub(crate) fn observe(&mut self, now_ms: u64, ok: bool) {
        if ok {
            if self.in_outage && self.healthy_since.is_none() {
                self.healthy_since = Some(now_ms);
            }
            return;
        }
        self.healthy_since = None;
        self.failures.push_back(now_ms);
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

    /// Try to return to the fleet path. True only when the replica was in
    /// outage state, Redis has answered without a failure for `recover_after_ms`
    /// and recovery (draw-down and release) has completed.
    pub(crate) fn try_resume(&mut self, now_ms: u64, recovery_complete: bool) -> bool {
        let steady = self
            .healthy_since
            .is_some_and(|since| now_ms.saturating_sub(since) >= self.recover_after_ms);
        if self.in_outage && steady && recovery_complete {
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
        if quote_nanos > self.unspent_nanos() {
            return Err(LocalRefusal::Exhausted);
        }
        self.reserved_nanos += quote_nanos;
        self.attempts.push(Attempt {
            id: attempt,
            quote: quote_nanos,
            charge: quote_nanos,
            cancelled: false,
        });
        Ok(())
    }

    /// An attempt that was never sent gives its quote back.
    pub(crate) fn cancel(&mut self, attempt: uuid::Uuid) {
        if let Some(a) = self
            .attempts
            .iter_mut()
            .find(|a| a.id == attempt && !a.cancelled)
        {
            a.cancelled = true;
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
}
