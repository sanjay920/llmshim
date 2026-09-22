//! Fleet-wide provider health.
//!
//! The in-process breaker in [`crate::breaker`] is the decision-maker; this
//! module gives it shared storage so one replica discovering that a provider is
//! down stops the rest of the fleet dialling it. It mirrors the split the rate
//! limiter already uses: in-memory by default, Redis when
//! `LLMSHIM_REDIS_URL` is set and the binary carries `redis-coordination`.

use crate::breaker::{BreakerConfig, ProviderBreaker};
use std::sync::Arc;

/// Build the process breaker, attaching the Redis-coordinated health store when
/// one is configured and available. Falls back to a purely local breaker with a
/// clear warning, exactly as `build_limiter` does.
pub fn build_breaker() -> Arc<ProviderBreaker> {
    let config = BreakerConfig::from_env();
    let breaker = ProviderBreaker::with_config(config);

    if let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") {
        let url = url.trim().to_string();
        if !url.is_empty() {
            #[cfg(feature = "redis-coordination")]
            {
                match redis_impl::RedisHealth::new(&url, config) {
                    Ok(shared) => {
                        eprintln!("provider health: redis coordination enabled");
                        return Arc::new(breaker.with_shared(Arc::new(shared)));
                    }
                    Err(e) => {
                        eprintln!(
                            "warning: LLMSHIM_REDIS_URL set but redis client init failed ({e}); \
                             provider health stays per-instance"
                        );
                    }
                }
            }
            #[cfg(not(feature = "redis-coordination"))]
            {
                eprintln!(
                    "warning: LLMSHIM_REDIS_URL is set but this binary was built without the \
                     'redis-coordination' feature; provider health stays per-instance."
                );
            }
        }
    }

    Arc::new(breaker)
}

#[cfg(feature = "redis-coordination")]
mod redis_impl {
    use super::*;
    use crate::breaker::{HealthFuture, SharedHealth};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Record one failure and decide whether the circuit is now open.
    ///
    /// Failures live in a sorted set scored by timestamp, so the sliding window
    /// is a range trim rather than a stored counter that has to be reset. The
    /// open marker holds `opened_at` with a TTL comfortably longer than the
    /// cooldown, so a probe can tell "open and cooling" from "open and due".
    const FAIL_LUA: &str = r#"
        local now      = tonumber(ARGV[1])
        local window   = tonumber(ARGV[2])
        local threshold= tonumber(ARGV[3])
        local cooldown = tonumber(ARGV[4])
        local member   = ARGV[5]

        redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now - window)
        redis.call('ZADD', KEYS[1], now, member)
        redis.call('PEXPIRE', KEYS[1], window)
        if redis.call('ZCARD', KEYS[1]) >= threshold then
            redis.call('SET', KEYS[2], now, 'PX', cooldown * 4)
            return 1
        end
        return 0
    "#;

    /// Reserve the single fleet-wide probe: only when the target is open, its
    /// cooldown has elapsed, and no other replica already took the slot.
    const PROBE_LUA: &str = r#"
        local now      = tonumber(ARGV[1])
        local cooldown = tonumber(ARGV[2])
        local opened = redis.call('GET', KEYS[1])
        if not opened then return 0 end
        if now - tonumber(opened) < cooldown then return 0 end
        if redis.call('SET', KEYS[2], now, 'NX', 'PX', cooldown) then
            return 1
        end
        return 0
    "#;

    /// Redis-backed [`SharedHealth`]. Every operation **fails open**: an
    /// unreachable coordinator reports healthy rather than taking every
    /// provider offline across the fleet.
    pub struct RedisHealth {
        connections: crate::redis_operation::RedisConnectionManagerCache,
        cfg: BreakerConfig,
        fail: redis::Script,
        probe: redis::Script,
    }

    impl RedisHealth {
        pub fn new(url: &str, cfg: BreakerConfig) -> redis::RedisResult<Self> {
            let client = redis::Client::open(url)?;
            Ok(Self {
                connections: crate::redis_operation::RedisConnectionManagerCache::new(client),
                cfg,
                fail: redis::Script::new(FAIL_LUA),
                probe: redis::Script::new(PROBE_LUA),
            })
        }

        fn failures_key(target: &str) -> String {
            format!("llmshim:health:{target}:failures")
        }
        fn open_key(target: &str) -> String {
            format!("llmshim:health:{target}:open")
        }
        fn probe_key(target: &str) -> String {
            format!("llmshim:health:{target}:probe")
        }

        fn now_ms() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }
    }

    impl SharedHealth for RedisHealth {
        fn is_open<'a>(&'a self, target: &'a str) -> HealthFuture<'a, bool> {
            Box::pin(async move {
                self.connections
                    .run(|mut connection| async move {
                        redis::cmd("EXISTS")
                            .arg(Self::open_key(target))
                            .query_async::<i64>(&mut connection)
                            .await
                    })
                    .await
                    .map(|n| n == 1)
                    .unwrap_or(false) // fail open
            })
        }

        fn try_admit_probe<'a>(&'a self, target: &'a str) -> HealthFuture<'a, bool> {
            Box::pin(async move {
                self.connections
                    .run(|mut connection| async move {
                        self.probe
                            .key(Self::open_key(target))
                            .key(Self::probe_key(target))
                            .arg(Self::now_ms())
                            .arg(self.cfg.cooldown.as_millis() as u64)
                            .invoke_async::<i64>(&mut connection)
                            .await
                    })
                    .await
                    .map(|n| n == 1)
                    .unwrap_or(true) // fail open: no coordinator, no gate
            })
        }

        fn observe<'a>(&'a self, target: &'a str, healthy: bool) -> HealthFuture<'a, ()> {
            Box::pin(async move {
                if healthy {
                    // Recovery clears the window and the open marker together,
                    // so one good response ends the fleet-wide skip.
                    let _ = self
                        .connections
                        .run(|mut connection| async move {
                            redis::cmd("DEL")
                                .arg(Self::failures_key(target))
                                .arg(Self::open_key(target))
                                .arg(Self::probe_key(target))
                                .query_async::<i64>(&mut connection)
                                .await
                        })
                        .await;
                    return;
                }
                let member = format!("{}:{}", Self::now_ms(), uuid::Uuid::new_v4().simple());
                let _ = self
                    .connections
                    .run(|mut connection| async move {
                        self.fail
                            .key(Self::failures_key(target))
                            .key(Self::open_key(target))
                            .arg(Self::now_ms())
                            .arg(self.cfg.window.as_millis() as u64)
                            .arg(self.cfg.trip_threshold as u64)
                            .arg(self.cfg.cooldown.as_millis() as u64)
                            .arg(member)
                            .invoke_async::<i64>(&mut connection)
                            .await
                    })
                    .await;
            })
        }
    }
}

#[cfg(feature = "redis-coordination")]
pub use redis_impl::RedisHealth;
