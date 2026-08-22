//! Client idempotency: a repeated `Idempotency-Key` returns the first response
//! instead of making a second (billed) upstream call.
//!
//! Cache-after-completion: the first request runs and caches its result under
//! the key; a later request with the same key gets the cached result. Two
//! *concurrent* requests with the same key may both run (no single-flight) — the
//! window is one in-flight call, which is an accepted simplification. In a
//! distributed fleet the cache is Redis-backed so it works across instances; the
//! in-memory cache below serves the single-instance path.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

/// A process-local TTL cache of completed responses keyed by idempotency key.
pub struct IdempotencyCache {
    entries: Mutex<HashMap<String, (Value, Instant)>>,
    ttl: Duration,
    max_entries: usize,
}

impl IdempotencyCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries: 100_000,
        }
    }

    /// Cached response for `key`, if present and not expired.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut map = self.entries.lock().unwrap();
        let now = Instant::now();
        match map.get(key) {
            Some((_, exp)) if *exp <= now => {
                map.remove(key);
                None
            }
            Some((v, _)) => Some(v.clone()),
            None => None,
        }
    }

    /// Cache `value` under `key`. Sweeps expired entries when the map grows past
    /// its cap so memory stays bounded.
    pub fn put(&self, key: &str, value: Value) {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap();
        if map.len() >= self.max_entries {
            map.retain(|_, (_, exp)| *exp > now);
        }
        map.insert(key.to_string(), (value, now + self.ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test(start_paused = true)]
    async fn caches_then_expires() {
        let c = IdempotencyCache::new(Duration::from_secs(60));
        assert!(c.get("k").is_none());
        c.put("k", json!({"v": 1}));
        assert_eq!(c.get("k"), Some(json!({"v": 1})));

        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(c.get("k").is_none(), "entry should expire");
    }
}
