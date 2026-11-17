//! Replica-owned spend leases (design note 0027, slice 2).
//!
//! A lease is a share of a tenant's window counter that one replica holds in
//! advance, while Redis is healthy, so it could admit locally during an outage.
//! This module is the ledger half only: nothing admits against a lease yet
//! (slices 3 to 5 build recovery and the local path), so the items are unused
//! outside the tests.
//!
//! The ledger counter is per tenant and window length
//! (`llmshim:gw:budget:v1:{tenant}:{window_secs}:{window_index}`), so a lease is
//! per (replica, tenant, window_secs, window_index). A lease is a reservation
//! *inside that counter*: taking one adds to the counter, releasing subtracts.
//! Expiry never returns money (a reservation in this ledger is only returned by
//! an explicit settle), so a dead replica's lease stays until the counter
//! itself expires with its window.
#![allow(dead_code)]

use crate::redis_operation::RedisConnectionManagerCache;

/// Largest value the ledger keeps exactly (shared with the attempt scripts).
const MAX_EXACT: u64 = crate::gateway::budget::MAX_EXACT_REDIS_NANOS;

/// Top a lease up (or down) to `fraction` of what the tenant has left.
/// KEYS[1] counter prefix, KEYS[2] lease prefix, KEYS[3] accounting index, KEYS[4] its
/// freeze marker; ARGV: replica, fraction, limit, window_secs, retention_secs,
/// max_exact, max_retained_attempts.
/// Returns {held, window_index, window_end_ms}, {-2} when the lease is closing, or
/// {-3} when the fleet's own admission is refusing (index frozen or full).
const TOP_UP_LUA: &str = r#"
    local t = redis.call('TIME')
    local now = t[1] * 1000 + math.floor(t[2] / 1000)
    local fraction = tonumber(ARGV[2])
    local limit = tonumber(ARGV[3])
    local window_secs = tonumber(ARGV[4])
    local retention_secs = tonumber(ARGV[5])
    local max_exact = tonumber(ARGV[6])
    local window_index = math.floor(math.floor(now / 1000) / window_secs)
    local window_end = (window_index + 1) * window_secs * 1000
    local ttl = math.max(1, window_end + retention_secs * 1000 - now)
    local total_key = KEYS[1] .. window_index
    local lease_key = KEYS[2] .. window_index .. ':' .. ARGV[1]
    if redis.call('HGET', lease_key, 'state') == 'closing' then
        return {-2}
    end
    if redis.call('EXISTS', KEYS[4]) == 1 or
            redis.call('ZCARD', KEYS[3]) >= tonumber(ARGV[7]) then
        return {-3}
    end
    local own = tonumber(redis.call('HGET', lease_key, 'held')) or 0
    if redis.call('EXISTS', total_key .. ':frozen') == 1 then
        return {own, window_index, window_end}
    end
    local total = tonumber(redis.call('GET', total_key)) or 0
    -- Spend by older binaries not yet applied to the counter counts as spent,
    -- as the fleet's own admission counts it.
    local known = tonumber(redis.call('GET', total_key .. ':legacy-known-floor')) or 0
    local applied = tonumber(redis.call('GET', total_key .. ':legacy-floor')) or 0
    local legacy_delta = math.max(0, known - applied)
    local avail = math.max(0, limit - total - legacy_delta + own)
    local target = math.min(avail, math.floor(avail * fraction))
    if target ~= own then
        total = total + (target - own)
        if total < 0 then total = 0 end
        if total > max_exact then return {own, window_index, window_end} end
        redis.call('SET', total_key, total, 'PX', math.max(ttl, redis.call('PTTL', total_key)))
        redis.call('HSET', lease_key, 'held', target, 'state', 'open',
            'window_end', window_end)
        redis.call('PEXPIRE', lease_key, ttl)
    end
    return {target, window_index, window_end}
"#;

/// Charge one locally admitted attempt to the lease, once per attempt id.
/// KEYS[1] lease, KEYS[2] draws, KEYS[3] counter; ARGV: attempt id, amount, max_exact.
/// Returns {held, excess} or {-2} when this attempt was drawn already.
/// A missing lease is {-1}.
const DRAW_LUA: &str = r#"
    if redis.call('HEXISTS', KEYS[2], ARGV[1]) == 1 then return {-2} end
    if redis.call('EXISTS', KEYS[1]) == 0 then return {-1} end
    local amount = tonumber(ARGV[2])
    local max_exact = tonumber(ARGV[3])
    local held = tonumber(redis.call('HGET', KEYS[1], 'held')) or 0
    local excess = 0
    if amount <= held then
        held = held - amount
    else
        excess = amount - held
        held = 0
        local total = tonumber(redis.call('GET', KEYS[3])) or 0
        if total > max_exact - excess then
            total = max_exact
            redis.call('SET', KEYS[3] .. ':frozen', 1,
                'PX', math.max(1, redis.call('PTTL', KEYS[1])))
        else
            total = total + excess
        end
        local counter_ttl = redis.call('PTTL', KEYS[3])
        if counter_ttl < 0 then counter_ttl = math.max(1, redis.call('PTTL', KEYS[1])) end
        redis.call('SET', KEYS[3], total, 'PX', counter_ttl)
    end
    redis.call('HSET', KEYS[1], 'held', held, 'state', 'closing')
    redis.call('HSET', KEYS[2], ARGV[1], amount)
    redis.call('PEXPIRE', KEYS[2], math.max(1, redis.call('PTTL', KEYS[1])))
    return {held, excess}
"#;

/// Return what is left of the lease to the counter and delete the lease.
/// KEYS[1] lease, KEYS[2] counter. Returns the amount returned, or -1 if absent.
const RELEASE_LUA: &str = r#"
    if redis.call('EXISTS', KEYS[1]) == 0 then return -1 end
    local held = tonumber(redis.call('HGET', KEYS[1], 'held')) or 0
    if redis.call('EXISTS', KEYS[2]) == 1 then
        local total = tonumber(redis.call('GET', KEYS[2])) or 0
        redis.call('SET', KEYS[2], math.max(0, total - held), 'KEEPTTL')
    end
    redis.call('DEL', KEYS[1])
    return held
"#;

/// A replica's lease for one (tenant, window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lease {
    /// Nanos of the tenant's window counter this replica holds, unspent.
    pub(crate) held_nanos: u64,
    pub(crate) window_index: u64,
    pub(crate) window_end_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseError {
    /// Redis was unreachable or answered unexpectedly.
    Unavailable,
    /// The lease is being drawn down for recovery; it takes no top-ups.
    Closing,
    /// The fleet's own admission is refusing (accounting index full or frozen).
    Refused,
    /// No such lease (never taken, or already released).
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Draw {
    pub(crate) held_nanos: u64,
    /// Nanos charged to the counter beyond the lease.
    pub(crate) excess_nanos: u64,
    /// False when this attempt id had been drawn before and nothing changed.
    pub(crate) applied: bool,
}

pub(crate) struct LeaseLedger {
    connections: RedisConnectionManagerCache,
    top_up: redis::Script,
    draw: redis::Script,
    release: redis::Script,
    retention_secs: u64,
    accounting_index_key: String,
    max_retained_attempts: usize,
}

impl LeaseLedger {
    /// `retention_secs`, `accounting_index_key` and `max_retained_attempts` must be
    /// the fleet attempt coordinator's own, so a lease is refused whenever the
    /// fleet's admission would be.
    pub(crate) fn new(
        url: &str,
        retention_secs: u64,
        accounting_index_key: String,
        max_retained_attempts: usize,
    ) -> redis::RedisResult<Self> {
        Ok(Self {
            connections: RedisConnectionManagerCache::new(redis::Client::open(url)?),
            top_up: redis::Script::new(TOP_UP_LUA),
            draw: redis::Script::new(DRAW_LUA),
            release: redis::Script::new(RELEASE_LUA),
            retention_secs,
            accounting_index_key,
            max_retained_attempts,
        })
    }

    fn counter_prefix(tenant_key: &str, window_secs: u64) -> String {
        format!("llmshim:gw:budget:v1:{tenant_key}:{window_secs}:")
    }

    fn lease_prefix(tenant_key: &str, window_secs: u64) -> String {
        format!("llmshim:gw:lease:v1:{tenant_key}:{window_secs}:")
    }

    fn lease_key(tenant_key: &str, window_secs: u64, window_index: u64, replica: &str) -> String {
        format!(
            "{}{window_index}:{replica}",
            Self::lease_prefix(tenant_key, window_secs)
        )
    }

    /// Take the lease, or move it to `fraction` of the tenant's remaining
    /// window. It sets the target, it never adds a fraction on top of the last
    /// one, so repeated renewals hold the lease where it is.
    pub(crate) async fn top_up(
        &self,
        tenant_key: &str,
        replica: &str,
        window_secs: u64,
        limit_nanos: u64,
        fraction: f64,
    ) -> Result<Lease, LeaseError> {
        if !(fraction > 0.0 && fraction <= 1.0) || window_secs == 0 || replica.contains(':') {
            return Err(LeaseError::Unavailable);
        }
        let reply: Vec<i64> = self
            .connections
            .run(|mut connection| async move {
                self.top_up
                    .key(Self::counter_prefix(tenant_key, window_secs))
                    .key(Self::lease_prefix(tenant_key, window_secs))
                    .key(&self.accounting_index_key)
                    .key(format!("{}:frozen", self.accounting_index_key))
                    .arg(replica)
                    .arg(fraction)
                    .arg(limit_nanos)
                    .arg(window_secs)
                    .arg(self.retention_secs)
                    .arg(MAX_EXACT)
                    .arg(self.max_retained_attempts)
                    .invoke_async(&mut connection)
                    .await
            })
            .await
            .map_err(|_| LeaseError::Unavailable)?;
        match reply.as_slice() {
            [-2] => Err(LeaseError::Closing),
            [-3] => Err(LeaseError::Refused),
            [held, index, end] if *held >= 0 && *index >= 0 && *end >= 0 => Ok(Lease {
                held_nanos: *held as u64,
                window_index: *index as u64,
                window_end_ms: *end as u64,
            }),
            _ => Err(LeaseError::Unavailable),
        }
    }

    /// Charge a locally admitted attempt's final amount to the lease, once.
    /// Calling it again with the same attempt id changes nothing, so a
    /// recovery that stops part-way resumes without charging twice.
    pub(crate) async fn draw(
        &self,
        tenant_key: &str,
        replica: &str,
        window_secs: u64,
        window_index: u64,
        attempt_id: uuid::Uuid,
        amount_nanos: u64,
    ) -> Result<Draw, LeaseError> {
        if replica.contains(':') {
            return Err(LeaseError::Unavailable);
        }
        let lease_key = Self::lease_key(tenant_key, window_secs, window_index, replica);
        let draws_key = format!("{lease_key}:draws");
        let counter_key = format!(
            "{}{window_index}",
            Self::counter_prefix(tenant_key, window_secs)
        );
        let reply: Vec<i64> = self
            .connections
            .run(|mut connection| async move {
                self.draw
                    .key(lease_key)
                    .key(draws_key)
                    .key(counter_key)
                    .arg(attempt_id.to_string())
                    .arg(amount_nanos.min(MAX_EXACT))
                    .arg(MAX_EXACT)
                    .invoke_async(&mut connection)
                    .await
            })
            .await
            .map_err(|_| LeaseError::Unavailable)?;
        match reply.as_slice() {
            [-1] => Err(LeaseError::Missing),
            [-2] => Ok(Draw {
                held_nanos: 0,
                excess_nanos: 0,
                applied: false,
            }),
            [held, excess] if *held >= 0 && *excess >= 0 => Ok(Draw {
                held_nanos: *held as u64,
                excess_nanos: *excess as u64,
                applied: true,
            }),
            _ => Err(LeaseError::Unavailable),
        }
    }

    /// Return the unspent remainder to the tenant's counter and delete the
    /// lease. Returns the amount given back.
    pub(crate) async fn release(
        &self,
        tenant_key: &str,
        replica: &str,
        window_secs: u64,
        window_index: u64,
    ) -> Result<u64, LeaseError> {
        let lease_key = Self::lease_key(tenant_key, window_secs, window_index, replica);
        let counter_key = format!(
            "{}{window_index}",
            Self::counter_prefix(tenant_key, window_secs)
        );
        let returned: i64 = self
            .connections
            .run(|mut connection| async move {
                self.release
                    .key(lease_key)
                    .key(counter_key)
                    .invoke_async(&mut connection)
                    .await
            })
            .await
            .map_err(|_| LeaseError::Unavailable)?;
        match returned {
            -1 => Err(LeaseError::Missing),
            n if n >= 0 => Ok(n as u64),
            _ => Err(LeaseError::Unavailable),
        }
    }

    #[cfg(test)]
    pub(crate) async fn counter(&self, tenant_key: &str, window_secs: u64, index: u64) -> u64 {
        let key = format!("{}{index}", Self::counter_prefix(tenant_key, window_secs));
        let mut connection = self.connections.connection_for_test().await.unwrap();
        redis::cmd("GET")
            .arg(key)
            .query_async::<Option<u64>>(&mut connection)
            .await
            .unwrap()
            .unwrap_or(0)
    }
}

/// What one replica admitted locally against one lease while Redis was out:
/// the final amount of each attempt, by attempt id.
#[derive(Clone, Debug)]
pub(crate) struct LocalLease {
    pub(crate) tenant_key: String,
    pub(crate) window_secs: u64,
    pub(crate) window_index: u64,
    pub(crate) attempts: Vec<(uuid::Uuid, u64)>,
}

/// How far a recovery got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecoveryReport {
    /// Leases fully drawn and released (or already gone).
    pub(crate) settled: usize,
    /// Leases left for a later pass because Redis failed part-way.
    pub(crate) pending: usize,
    /// Nanos charged beyond the leases (settlement above the reservation).
    pub(crate) excess_nanos: u64,
    /// Nanos given back to the counters.
    pub(crate) released_nanos: u64,
}

impl RecoveryReport {
    pub(crate) fn complete(&self) -> bool {
        self.pending == 0
    }
}

impl LeaseLedger {
    /// Settle every local lease: draw each attempt (idempotent per attempt id),
    /// then release the remainder. Safe to repeat after any interruption: a
    /// pass that stopped part-way is resumed by running it again with the same
    /// records, and nothing is charged twice. A lease whose Redis calls fail is
    /// left whole and counted as pending; the others still proceed.
    pub(crate) async fn recover(&self, replica: &str, leases: &[LocalLease]) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        for lease in leases {
            let mut excess = 0;
            let mut ok = true;
            for (attempt, amount) in &lease.attempts {
                match self
                    .draw(
                        &lease.tenant_key,
                        replica,
                        lease.window_secs,
                        lease.window_index,
                        *attempt,
                        *amount,
                    )
                    .await
                {
                    Ok(draw) => excess += draw.excess_nanos,
                    // The lease is gone: an earlier pass released it after
                    // drawing these attempts.
                    Err(LeaseError::Missing) => {}
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                report.pending += 1;
                continue;
            }
            match self
                .release(
                    &lease.tenant_key,
                    replica,
                    lease.window_secs,
                    lease.window_index,
                )
                .await
            {
                Ok(returned) => report.released_nanos += returned,
                Err(LeaseError::Missing) => {}
                Err(_) => {
                    report.pending += 1;
                    report.excess_nanos += excess;
                    continue;
                }
            }
            report.settled += 1;
            report.excess_nanos += excess;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: u64 = 1_000_000_000;
    const WINDOW: u64 = 3600;

    fn ledger() -> Option<LeaseLedger> {
        let url = std::env::var("LLMSHIM_REDIS_URL").ok()?;
        Some(
            LeaseLedger::new(
                &url,
                60,
                format!("llmshim:gw:test-index:{}", uuid::Uuid::new_v4().simple()),
                1000,
            )
            .unwrap(),
        )
    }

    fn tenant() -> String {
        format!("lease-test-{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_lease_is_reserved_in_the_tenants_counter() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        assert_eq!(lease.held_nanos, 100_000_000);
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 100_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn renewal_sets_a_target_and_never_compounds() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let mut last = None;
        for _ in 0..20 {
            last = Some(l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap());
        }
        let lease = last.unwrap();
        assert_eq!(lease.held_nanos, 100_000_000);
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 100_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn replicas_share_the_remainder_and_never_exceed_the_limit() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let mut index = 0;
        for replica in 0..40 {
            let lease = l
                .top_up(&t, &format!("r{replica}"), WINDOW, LIMIT, 0.5)
                .await
                .unwrap();
            index = lease.window_index;
        }
        // Each takes half of what is left: the counter approaches but never passes the limit.
        let counter = l.counter(&t, WINDOW, index).await;
        assert!(counter <= LIMIT, "{counter}");
        assert!(counter > LIMIT / 2);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn the_counter_is_per_window_length() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let a = l.top_up(&t, "a", WINDOW, LIMIT, 0.2).await.unwrap();
        l.top_up(&t, "a", 7200, LIMIT, 0.2).await.unwrap();
        assert_eq!(l.counter(&t, WINDOW, a.window_index).await, 200_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn draw_is_idempotent_per_attempt_and_release_returns_the_rest() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        let i = lease.window_index;
        let id = uuid::Uuid::new_v4();
        let first = l.draw(&t, "a", WINDOW, i, id, 30_000_000).await.unwrap();
        assert_eq!(
            first,
            Draw {
                held_nanos: 70_000_000,
                excess_nanos: 0,
                applied: true
            }
        );
        let again = l.draw(&t, "a", WINDOW, i, id, 30_000_000).await.unwrap();
        assert!(!again.applied);
        // Spent 30M stays in the counter; the 70M remainder goes back.
        assert_eq!(l.release(&t, "a", WINDOW, i).await.unwrap(), 70_000_000);
        assert_eq!(l.counter(&t, WINDOW, i).await, 30_000_000);
        // A resumed recovery repeating a draw after release is still a no-op.
        let late = l.draw(&t, "a", WINDOW, i, id, 30_000_000).await.unwrap();
        assert!(!late.applied);
        assert_eq!(l.counter(&t, WINDOW, i).await, 30_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_draw_above_the_lease_charges_the_excess_to_the_counter() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        let i = lease.window_index;
        let d = l
            .draw(&t, "a", WINDOW, i, uuid::Uuid::new_v4(), 130_000_000)
            .await
            .unwrap();
        assert_eq!((d.held_nanos, d.excess_nanos), (0, 30_000_000));
        assert_eq!(l.counter(&t, WINDOW, i).await, 130_000_000);
        assert_eq!(l.release(&t, "a", WINDOW, i).await.unwrap(), 0);
        assert_eq!(l.counter(&t, WINDOW, i).await, 130_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_closing_lease_takes_no_top_up() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        l.draw(&t, "a", WINDOW, lease.window_index, uuid::Uuid::new_v4(), 1)
            .await
            .unwrap();
        assert_eq!(
            l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await,
            Err(LeaseError::Closing)
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn an_absent_lease_is_reported_and_a_stranded_one_stays_in_the_counter() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        assert_eq!(
            l.release(&t, "ghost", WINDOW, 1).await,
            Err(LeaseError::Missing)
        );
        // A replica that never releases (it died) keeps its share in the counter.
        let lease = l.top_up(&t, "dead", WINDOW, LIMIT, 0.1).await.unwrap();
        let other = l.top_up(&t, "live", WINDOW, LIMIT, 0.1).await.unwrap();
        assert_eq!(other.held_nanos, 90_000_000);
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 190_000_000);
    }

    async fn raw(l: &LeaseLedger) -> redis::aio::ConnectionManager {
        l.connections.connection_for_test().await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn unapplied_legacy_spend_is_not_leased() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let first = l.top_up(&t, "a", WINDOW, LIMIT, 0.5).await.unwrap();
        let key = format!(
            "{}{}:legacy-known-floor",
            LeaseLedger::counter_prefix(&t, WINDOW),
            first.window_index
        );
        let mut c = raw(&l).await;
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg(800_000_000_u64)
            .query_async(&mut c)
            .await
            .unwrap();
        // 500M held and 800M unapplied leave 200M of headroom (own share included),
        // so half of it is 100M, not half of 500M.
        let again = l.top_up(&t, "a", WINDOW, LIMIT, 0.5).await.unwrap();
        assert_eq!(again.held_nanos, 100_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn no_lease_while_the_fleet_accounting_is_refusing() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let mut c = raw(&l).await;
        let _: () = redis::cmd("SET")
            .arg(format!("{}:frozen", l.accounting_index_key))
            .arg(1)
            .query_async(&mut c)
            .await
            .unwrap();
        assert_eq!(
            l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await,
            Err(LeaseError::Refused)
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_lost_counter_is_not_recreated_without_expiry() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        let counter = format!(
            "{}{}",
            LeaseLedger::counter_prefix(&t, WINDOW),
            lease.window_index
        );
        let mut c = raw(&l).await;
        let _: () = redis::cmd("DEL")
            .arg(&counter)
            .query_async(&mut c)
            .await
            .unwrap();
        l.draw(
            &t,
            "a",
            WINDOW,
            lease.window_index,
            uuid::Uuid::new_v4(),
            150_000_000,
        )
        .await
        .unwrap();
        let ttl: i64 = redis::cmd("PTTL")
            .arg(&counter)
            .query_async(&mut c)
            .await
            .unwrap();
        assert!(ttl > 0, "{ttl}");
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn a_replica_id_cannot_collide_with_the_draws_key() {
        let Some(l) = ledger() else { return };
        assert_eq!(
            l.top_up(&tenant(), "a:draws", WINDOW, LIMIT, 0.1).await,
            Err(LeaseError::Unavailable)
        );
    }

    fn local(t: &str, index: u64, amounts: &[u64]) -> LocalLease {
        LocalLease {
            tenant_key: t.to_string(),
            window_secs: WINDOW,
            window_index: index,
            attempts: amounts.iter().map(|a| (uuid::Uuid::new_v4(), *a)).collect(),
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn recovery_draws_then_releases_and_a_repeat_changes_nothing() {
        let Some(l) = ledger() else { return };
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        let records = [local(&t, lease.window_index, &[20_000_000, 90_000_000])];
        let report = l.recover("a", &records).await;
        assert!(report.complete());
        assert_eq!(report.excess_nanos, 10_000_000);
        assert_eq!(report.released_nanos, 0);
        // 110M spent: 100M of lease plus 10M excess; nothing is held back.
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 110_000_000);
        let again = l.recover("a", &records).await;
        assert!(again.complete());
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 110_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn an_interrupted_recovery_resumes_to_the_uninterrupted_result() {
        let Some(l) = ledger() else { return };
        let (a, b) = (tenant(), tenant());
        let la = l.top_up(&a, "r", WINDOW, LIMIT, 0.1).await.unwrap();
        let lb = l.top_up(&b, "r", WINDOW, LIMIT, 0.1).await.unwrap();
        let ra = local(&a, la.window_index, &[10_000_000, 30_000_000, 5_000_000]);
        let rb = local(&b, lb.window_index, &[10_000_000, 30_000_000, 5_000_000]);
        // A: stopped after the first draw (what a dropped connection leaves).
        l.draw(
            &a,
            "r",
            WINDOW,
            la.window_index,
            ra.attempts[0].0,
            ra.attempts[0].1,
        )
        .await
        .unwrap();
        let resumed = l.recover("r", std::slice::from_ref(&ra)).await;
        // B: never interrupted.
        let whole = l.recover("r", std::slice::from_ref(&rb)).await;
        assert!(resumed.complete() && whole.complete());
        assert_eq!(
            l.counter(&a, WINDOW, la.window_index).await,
            l.counter(&b, WINDOW, lb.window_index).await
        );
        assert_eq!(l.counter(&a, WINDOW, la.window_index).await, 45_000_000);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn recovery_with_redis_down_leaves_everything_pending() {
        let Some(l) = ledger() else { return };
        let dead = LeaseLedger::new("redis://127.0.0.1:1", 60, "idx".into(), 10).unwrap();
        let t = tenant();
        let lease = l.top_up(&t, "a", WINDOW, LIMIT, 0.1).await.unwrap();
        let records = [local(&t, lease.window_index, &[1_000_000])];
        let report = dead.recover("a", &records).await;
        assert_eq!((report.pending, report.settled), (1, 0));
        // Nothing moved, and the real ledger still finishes it.
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 100_000_000);
        assert!(l.recover("a", &records).await.complete());
        assert_eq!(l.counter(&t, WINDOW, lease.window_index).await, 1_000_000);
    }
}
