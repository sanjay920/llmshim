//! Provider circuit breaking.
//!
//! This is **provider health**, not rate-limit backoff. The token buckets in
//! `crate::proxy::ratelimit` already slow a provider down after a 429; a 429
//! means the provider is alive and asking for less traffic, so it never counts
//! here. What counts is a target that is failing in a way retrying cannot fix
//! — 5xx and transport failures — and the answer to that is to stop sending,
//! not to send more slowly.
//!
//! The breaker uses a sliding failure window, an open state, and a single
//! half-open probe admitted after the cooldown. Failures are classified from
//! [`ShimError`], and the local registry can be paired with a [`SharedHealth`]
//! backend so a fleet agrees on which providers are down.

use crate::error::ShimError;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Statuses that indicate the target itself is unhealthy. 429 is deliberately
/// absent: it is a rate-limit signal, handled by the limiter's backoff.
const UNHEALTHY_STATUSES: &[u16] = &[500, 502, 503, 504, 529];

/// Whether a failure says anything about the target's *health*.
pub fn counts_toward_health(error: &ShimError) -> bool {
    match error {
        ShimError::ProviderError { status, .. } => UNHEALTHY_STATUSES.contains(status),
        // Connect/timeout/body failures reached nothing usable.
        ShimError::Http(_) => true,
        _ => false,
    }
}

/// Configuration for a [`ProviderBreaker`].
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Sliding window in which eligible failures are counted.
    pub window: Duration,
    /// Eligible failures required to open a target's circuit. `0` disables the
    /// breaker entirely.
    pub trip_threshold: usize,
    /// Time an open circuit stays unavailable before admitting a probe.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            trip_threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }
}

impl BreakerConfig {
    /// Read `LLMSHIM_BREAKER_WINDOW_SECS`, `LLMSHIM_BREAKER_TRIP_THRESHOLD` and
    /// `LLMSHIM_BREAKER_COOLDOWN_SECS`, keeping the defaults for anything unset
    /// or unparseable. `LLMSHIM_BREAKER_TRIP_THRESHOLD=0` turns it off.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            window: env_secs("LLMSHIM_BREAKER_WINDOW_SECS").unwrap_or(default.window),
            trip_threshold: std::env::var("LLMSHIM_BREAKER_TRIP_THRESHOLD")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default.trip_threshold),
            cooldown: env_secs("LLMSHIM_BREAKER_COOLDOWN_SECS").unwrap_or(default.cooldown),
        }
    }

    fn is_disabled(&self) -> bool {
        self.trip_threshold == 0
    }
}

fn env_secs(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

enum TargetState {
    Closed { failures: VecDeque<Instant> },
    Open { opened_at: Instant },
    HalfOpen { opened_at: Instant },
}

/// A boxed future, so the shared backend can be async without pulling
/// `async-trait` into the default build.
pub type HealthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A cross-instance view of provider health.
///
/// Implementations back the same open/half-open state with shared storage, so
/// one replica discovering a dead provider stops the whole fleet dialling it.
/// A backend that cannot be reached must fail **open** (report healthy) — a
/// coordination outage should not take every provider offline.
pub trait SharedHealth: Send + Sync {
    /// Whether `target` should be skipped fleet-wide right now.
    fn is_open<'a>(&'a self, target: &'a str) -> HealthFuture<'a, bool>;
    /// Reserve the single fleet-wide probe for an open target.
    fn try_admit_probe<'a>(&'a self, target: &'a str) -> HealthFuture<'a, bool>;
    /// Record one outcome against the shared window.
    fn observe<'a>(&'a self, target: &'a str, healthy: bool) -> HealthFuture<'a, ()>;
}

/// Sliding-window circuit breaker registry keyed by provider target.
pub struct ProviderBreaker {
    cfg: BreakerConfig,
    targets: Mutex<HashMap<String, TargetState>>,
    shared: Option<std::sync::Arc<dyn SharedHealth>>,
}

impl Default for ProviderBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderBreaker {
    /// A breaker with the default configuration, governing this process only.
    pub fn new() -> Self {
        Self::with_config(BreakerConfig::default())
    }

    /// A breaker configured from the environment, governing this process only.
    pub fn from_env() -> Self {
        Self::with_config(BreakerConfig::from_env())
    }

    /// A breaker with an explicit configuration.
    pub fn with_config(cfg: BreakerConfig) -> Self {
        Self {
            cfg,
            targets: Mutex::new(HashMap::new()),
            shared: None,
        }
    }

    /// Pair the local registry with a fleet-wide backend. Local state still
    /// applies: an instance that has seen a target fail skips it immediately,
    /// without waiting for the shared view to agree.
    pub fn with_shared(mut self, shared: std::sync::Arc<dyn SharedHealth>) -> Self {
        self.shared = Some(shared);
        self
    }

    pub fn config(&self) -> BreakerConfig {
        self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TargetState>> {
        self.targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether `target` is currently unavailable through the breaker.
    ///
    /// Read-only. An open target stays skippable after its cooldown until one
    /// caller reserves the probe with [`Self::try_admit_probe`].
    pub fn should_skip(&self, target: &str) -> bool {
        matches!(
            self.lock().get(target),
            Some(TargetState::Open { .. } | TargetState::HalfOpen { .. })
        )
    }

    /// Reserve the single half-open probe slot for `target`.
    ///
    /// `true` only when the target was open, its cooldown had elapsed, and this
    /// caller moved it to half-open. The caller must then report the outcome
    /// with [`Self::record_success`] or [`Self::record_failure`].
    pub fn try_admit_probe(&self, target: &str, now: Instant) -> bool {
        let mut targets = self.lock();
        let Some(state) = targets.get_mut(target) else {
            return false;
        };
        let TargetState::Open { opened_at } = state else {
            return false;
        };
        if now.saturating_duration_since(*opened_at) < self.cfg.cooldown {
            return false;
        }
        *state = TargetState::HalfOpen {
            opened_at: *opened_at,
        };
        true
    }

    /// Close `target` and clear its failure window.
    pub fn record_success(&self, target: &str) {
        self.lock().remove(target);
    }

    /// Record a failure for `target`. `counts` is the classification from
    /// [`counts_toward_health`] — an ineligible failure never opens a circuit,
    /// but it does return a half-open target to open so a probe is not
    /// mistaken for recovery.
    pub fn record_failure(&self, target: &str, counts: bool, now: Instant) {
        if self.cfg.is_disabled() {
            return;
        }
        let mut targets = self.lock();

        if !counts {
            if let Some(TargetState::HalfOpen { opened_at }) = targets.get(target) {
                let opened_at = *opened_at;
                targets.insert(target.to_owned(), TargetState::Open { opened_at });
            }
            return;
        }

        match targets.entry(target.to_owned()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                let failures = VecDeque::from([now]);
                if failures.len() >= self.cfg.trip_threshold {
                    entry.insert(TargetState::Open { opened_at: now });
                } else {
                    entry.insert(TargetState::Closed { failures });
                }
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => match entry.get_mut() {
                TargetState::Closed { failures } => {
                    while failures.front().is_some_and(|failure_at| {
                        now.saturating_duration_since(*failure_at) > self.cfg.window
                    }) {
                        failures.pop_front();
                    }
                    failures.push_back(now);
                    if failures.len() >= self.cfg.trip_threshold {
                        entry.insert(TargetState::Open { opened_at: now });
                    }
                }
                TargetState::Open { .. } | TargetState::HalfOpen { .. } => {
                    entry.insert(TargetState::Open { opened_at: now });
                }
            },
        }
    }

    /// Ask whether a dispatch to `target` may proceed, consulting the fleet
    /// view when one is configured.
    ///
    /// `false` means the circuit is open and no probe was available: the caller
    /// should skip this target rather than retry into a known-dead one.
    pub async fn admit(&self, target: &str) -> bool {
        if self.cfg.is_disabled() {
            return true;
        }
        let now = Instant::now();
        if self.should_skip(target) {
            // Local state is open; a probe may still be due.
            return self.try_admit_probe(target, now);
        }
        let Some(shared) = &self.shared else {
            return true;
        };
        if !shared.is_open(target).await {
            return true;
        }
        shared.try_admit_probe(target).await
    }

    /// Record a dispatch outcome locally and, when configured, fleet-wide.
    pub async fn observe(&self, target: &str, outcome: Result<(), &ShimError>) {
        match outcome {
            Ok(()) => self.record_success(target),
            Err(error) => self.record_failure(target, counts_toward_health(error), Instant::now()),
        }
        if let Some(shared) = &self.shared {
            let healthy = match outcome {
                Ok(()) => true,
                // Only health-relevant failures move the shared window; a 400
                // from one caller must not take a provider down for everyone.
                Err(error) => !counts_toward_health(error),
            };
            shared.observe(target, healthy).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BreakerConfig {
        BreakerConfig {
            window: Duration::from_secs(10),
            trip_threshold: 3,
            cooldown: Duration::from_secs(5),
        }
    }

    fn unhealthy() -> ShimError {
        ShimError::ProviderError {
            status: 503,
            body: "down".into(),
            retry_after: None,
        }
    }

    fn trip(breaker: &ProviderBreaker, target: &str, now: Instant) {
        for offset in 0..3 {
            breaker.record_failure(target, true, now + Duration::from_secs(offset));
        }
    }

    #[test]
    fn trips_open_after_threshold_unhealthy_failures() {
        let breaker = ProviderBreaker::with_config(config());
        trip(&breaker, "anthropic", Instant::now());
        assert!(breaker.should_skip("anthropic"));
    }

    #[test]
    fn rate_limiting_is_not_a_health_signal() {
        // 429 is what the token buckets are for. Counting it here would open a
        // circuit on a provider that is up and merely busy.
        assert!(!counts_toward_health(&ShimError::ProviderError {
            status: 429,
            body: "slow down".into(),
            retry_after: None,
        }));
        for status in [500, 502, 503, 504, 529] {
            assert!(counts_toward_health(&ShimError::ProviderError {
                status,
                body: String::new(),
                retry_after: None,
            }));
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!counts_toward_health(&ShimError::ProviderError {
                status,
                body: String::new(),
                retry_after: None,
            }));
        }
        assert!(!counts_toward_health(&ShimError::MissingModel));
    }

    #[test]
    fn ineligible_failures_never_trip() {
        let breaker = ProviderBreaker::with_config(config());
        let now = Instant::now();
        for offset in 0..12 {
            breaker.record_failure("openai", false, now + Duration::from_millis(offset));
        }
        assert!(!breaker.should_skip("openai"));
    }

    #[test]
    fn window_expiry_drops_old_failures() {
        let breaker = ProviderBreaker::with_config(config());
        let now = Instant::now();
        for offset in [0, 11, 22, 33] {
            let at = now + Duration::from_secs(offset);
            breaker.record_failure("openai", true, at);
            assert!(!breaker.should_skip("openai"));
        }
    }

    #[test]
    fn cooldown_admits_exactly_one_probe() {
        let breaker = ProviderBreaker::with_config(config());
        let now = Instant::now();
        trip(&breaker, "gemini", now);
        let opened_at = now + Duration::from_secs(2);

        assert!(breaker.should_skip("gemini"));
        assert!(!breaker.try_admit_probe("gemini", opened_at + Duration::from_secs(2)));
        assert!(breaker.try_admit_probe("gemini", opened_at + Duration::from_secs(5)));
        assert!(
            !breaker.try_admit_probe("gemini", opened_at + Duration::from_secs(5)),
            "the probe slot is single-occupancy"
        );
    }

    #[test]
    fn probe_success_closes_and_probe_failure_reopens() {
        let breaker = ProviderBreaker::with_config(config());
        let now = Instant::now();
        trip(&breaker, "xai", now);
        let probe_at = now + Duration::from_secs(7);
        assert!(breaker.try_admit_probe("xai", probe_at));
        breaker.record_success("xai");
        assert!(!breaker.should_skip("xai"));

        trip(&breaker, "xai", probe_at);
        let probe_at = probe_at + Duration::from_secs(7);
        assert!(breaker.try_admit_probe("xai", probe_at));
        breaker.record_failure("xai", true, probe_at);
        assert!(!breaker.try_admit_probe("xai", probe_at + Duration::from_secs(4)));
        assert!(breaker.try_admit_probe("xai", probe_at + Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn admit_and_observe_drive_the_whole_cycle() {
        let breaker = ProviderBreaker::with_config(BreakerConfig {
            window: Duration::from_secs(10),
            trip_threshold: 2,
            cooldown: Duration::from_secs(5),
        });
        assert!(breaker.admit("anthropic").await);
        breaker.observe("anthropic", Err(&unhealthy())).await;
        assert!(breaker.admit("anthropic").await);
        breaker.observe("anthropic", Err(&unhealthy())).await;
        assert!(
            !breaker.admit("anthropic").await,
            "a known-dead provider must be skipped, not retried into"
        );
        breaker.observe("anthropic", Ok(())).await;
        assert!(breaker.admit("anthropic").await, "success closes it again");
    }

    #[tokio::test]
    async fn a_disabled_breaker_admits_everything() {
        let breaker = ProviderBreaker::with_config(BreakerConfig {
            trip_threshold: 0,
            ..config()
        });
        for _ in 0..50 {
            assert!(breaker.admit("anthropic").await);
            breaker.observe("anthropic", Err(&unhealthy())).await;
        }
        assert!(breaker.admit("anthropic").await);
    }

    #[tokio::test]
    async fn a_shared_backend_can_open_a_locally_healthy_target() {
        struct AlwaysOpen;
        impl SharedHealth for AlwaysOpen {
            fn is_open<'a>(&'a self, _: &'a str) -> HealthFuture<'a, bool> {
                Box::pin(async { true })
            }
            fn try_admit_probe<'a>(&'a self, _: &'a str) -> HealthFuture<'a, bool> {
                Box::pin(async { false })
            }
            fn observe<'a>(&'a self, _: &'a str, _: bool) -> HealthFuture<'a, ()> {
                Box::pin(async {})
            }
        }
        let breaker =
            ProviderBreaker::with_config(config()).with_shared(std::sync::Arc::new(AlwaysOpen));
        assert!(
            !breaker.admit("anthropic").await,
            "another replica's finding must reach this one"
        );
    }
}
