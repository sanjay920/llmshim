//! Redis-backed **distributed gateway** (features `gateway` + `redis-coordination`).
//!
//! The in-memory [`Scheduler`](super::Scheduler) governs one process. A fleet of
//! gateway replicas (Cloud Run / ECS) needs a *shared* priority queue so any
//! instance can serve any request, ordered globally by tier then FIFO, while the
//! shared [`RedisRateLimiter`](crate::proxy::ratelimit) keeps the whole fleet
//! under one provider rate limit.
//!
//! As the advisor put it, you can't stretch the in-process `Job`/`RequestQueue`
//! (it holds a `oneshot`) across processes — so distributed mode is a **separate
//! seam** with two pieces:
//!
//! * a **serializable job queue** — a Redis sorted set per provider, scored so
//!   `ZPOPMAX` yields the highest-priority (then earliest) job to whichever
//!   worker polls first; and
//! * a **response bus** — Redis pub/sub on a per-request channel, so the worker
//!   that dispatches a job publishes the result back to the *origin* instance's
//!   open HTTP connection.
//!
//! ```text
//!   instance A: submit ──subscribe(resp:ID)──enqueue(ZADD q:prov)─┐  await bus
//!                                                                  ▼
//!   shared Redis:                       [ ZSET q:prov ]   [ pub/sub resp:ID ]
//!                                                                  ▲
//!   instance B: worker ──ZPOPMAX q:prov ──rate-limit──dispatch──publish(resp:ID)
//! ```
//!
//! MVP scope: **unary** requests (streaming-through-the-bus is deferred — use a
//! single instance for streaming). At-least-once is *not* guaranteed: a worker
//! that crashes mid-dispatch drops that one request (the origin times out); a
//! visibility-timeout lease + reaper is the follow-up. Priority is tier+FIFO
//! (aging is an in-memory-lane concern).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::{Dispatch, GatewayConfig, GatewayError, GatewayRequest};
use crate::proxy::ratelimit::{RateKey, RateLimiter, RetryAfter};

/// Bits of the score reserved for the FIFO sequence; the rest carry the tier.
/// `tier << SEQ_BITS | (SEQ_MASK - seq)` stays < 2^53, so it round-trips through
/// a Redis `f64` score exactly (tiers 0..=255, ~3.5e13 sequence values).
const SEQ_BITS: u32 = 45;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;

/// Score for a job so `ZPOPMAX` (highest score first) yields higher tiers
/// first, and within a tier the earlier (smaller `seq`) job first.
fn priority_score(tier: u8, seq: u64) -> f64 {
    (((tier as u64) << SEQ_BITS) | (SEQ_MASK - (seq & SEQ_MASK))) as f64
}

fn queue_key(provider: &str) -> String {
    format!("llmshim:gw:q:{provider}")
}

fn response_channel(id: &str) -> String {
    format!("llmshim:gw:resp:{id}")
}

/// A queued unit of work, serialized into the Redis sorted set.
#[derive(Serialize, Deserialize)]
struct JobDescriptor {
    id: String,
    provider: String,
    tier: u8,
    permits: u32,
    payload: Value,
}

/// The result published back over the response bus.
#[derive(Serialize, Deserialize)]
#[serde(tag = "status", content = "body")]
enum JobResult {
    Ok(Value),
    Err(String),
}

/// A Redis-backed distributed gateway. One per instance; runs the origin side
/// (`submit`) and the worker side ([`spawn_workers`](Self::spawn_workers)).
pub struct DistributedGateway {
    client: redis::Client,
    conn: ConnectionManager,
    dispatch: Arc<dyn Dispatch>,
    limiter: Arc<dyn RateLimiter>,
    config: GatewayConfig,
    /// Per-instance nonce making request ids globally unique.
    nonce: u128,
    counter: AtomicU64,
}

impl DistributedGateway {
    /// Connect to Redis and build the gateway. The rate limiter should be the
    /// shared [`RedisRateLimiter`] so the whole fleet coordinates.
    pub async fn connect(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
    ) -> redis::RedisResult<Arc<Self>> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client.clone()).await?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(Arc::new(Self {
            client,
            conn,
            dispatch,
            limiter,
            config,
            nonce,
            counter: AtomicU64::new(0),
        }))
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", self.nonce, n)
    }

    /// Origin side: enqueue by priority and await the result over the bus.
    pub async fn submit(&self, req: GatewayRequest) -> Result<Value, GatewayError> {
        let id = self.next_id();
        let desc = JobDescriptor {
            id: id.clone(),
            provider: req.provider.clone(),
            tier: req.tier,
            permits: req.permits.max(1),
            payload: req.payload,
        };
        let member = serde_json::to_string(&desc).map_err(|e| redis_err(&e))?;
        let channel = response_channel(&id);

        // Subscribe BEFORE enqueue so we can't miss the (fire-and-forget) publish.
        let mut pubsub = self
            .client
            .get_async_pubsub()
            .await
            .map_err(|e| redis_err(&e))?;
        pubsub
            .subscribe(&channel)
            .await
            .map_err(|e| redis_err(&e))?;

        // Global FIFO sequence + priority score, then enqueue.
        let mut conn = self.conn.clone();
        let seq: u64 = conn
            .incr("llmshim:gw:seq", 1)
            .await
            .map_err(|e| redis_err(&e))?;
        let key = queue_key(&req.provider);
        let _: () = conn
            .zadd(&key, &member, priority_score(req.tier, seq))
            .await
            .map_err(|e| redis_err(&e))?;

        // Await the result on the bus, bounded by the total request timeout.
        use futures::StreamExt;
        let mut messages = pubsub.on_message();
        match tokio::time::timeout(self.config.request_timeout, messages.next()).await {
            Ok(Some(msg)) => {
                let payload: String = msg.get_payload().map_err(|e| redis_err(&e))?;
                match serde_json::from_str::<JobResult>(&payload) {
                    Ok(JobResult::Ok(v)) => Ok(v),
                    Ok(JobResult::Err(e)) => Err(GatewayError::Upstream(e)),
                    Err(e) => Err(GatewayError::Upstream(format!(
                        "bad response envelope: {e}"
                    ))),
                }
            }
            Ok(None) => Err(GatewayError::Shutdown),
            Err(_) => {
                // Timed out — best-effort remove from the queue if still waiting.
                let _: Result<i64, _> = conn.zrem(&key, &member).await;
                Err(GatewayError::Timeout)
            }
        }
    }

    /// Worker side: spawn one dispatcher loop per provider. Each polls
    /// `ZPOPMAX` for the highest-priority job, respects the shared rate limit,
    /// dispatches, and publishes the result to the origin.
    pub fn spawn_workers(self: &Arc<Self>, providers: Vec<String>) -> Vec<JoinHandle<()>> {
        providers
            .into_iter()
            .map(|provider| {
                let me = self.clone();
                tokio::spawn(async move { me.worker(provider).await })
            })
            .collect()
    }

    async fn worker(self: Arc<Self>, provider: String) {
        // Idle poll interval — a blocking BZPOPMAX would trip the multiplexed
        // connection's response timeout and desync it, so we poll the
        // non-blocking ZPOPMAX instead (a keyspace-notification wakeup is a
        // possible future optimization).
        const IDLE_POLL: Duration = Duration::from_millis(50);

        let key = queue_key(&provider);
        let rate_key = RateKey::provider(provider.clone());
        let sem = Arc::new(Semaphore::new(
            self.config.max_concurrency_per_provider.max(1),
        ));
        let mut conn = self.conn.clone();

        loop {
            // Atomically pop the single highest-priority job (tier, then FIFO).
            let popped: Vec<(String, f64)> = match conn.zpopmax(&key, 1).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("gateway worker[{provider}]: ZPOPMAX error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let Some((member, score)) = popped.into_iter().next() else {
                tokio::time::sleep(IDLE_POLL).await; // queue empty
                continue;
            };
            let desc: JobDescriptor = match serde_json::from_str(&member) {
                Ok(d) => d,
                Err(_) => continue, // skip a corrupt entry
            };

            match self.limiter.acquire(&rate_key, desc.permits).await {
                Ok(()) => {
                    let permit = match sem.clone().acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => break,
                    };
                    let me = self.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        me.run_and_publish(desc).await;
                    });
                }
                Err(RetryAfter(wait)) => {
                    // Requeue with the same score (priority preserved) and back off
                    // — capped so we re-poll promptly when a token refills.
                    let _: Result<i64, _> = conn.zadd(&key, &member, score).await;
                    tokio::time::sleep(wait.min(Duration::from_millis(500))).await;
                }
            }
        }
    }

    /// Dispatch a leased job and publish the result to its response channel.
    async fn run_and_publish(&self, desc: JobDescriptor) {
        let channel = response_channel(&desc.id);
        let provider = desc.provider.clone();
        let result = match self.dispatch.dispatch(&provider, desc.payload).await {
            Ok(value) => JobResult::Ok(value),
            Err(err) => {
                if let Some(retry_after) = err.retry_after {
                    self.limiter
                        .penalize(&RateKey::provider(provider), retry_after)
                        .await;
                }
                JobResult::Err(err.message)
            }
        };
        if let Ok(payload) = serde_json::to_string(&result) {
            let mut conn = self.conn.clone();
            let _: Result<i64, _> = conn.publish(&channel, payload).await;
        }
    }
}

/// Wrap any error as a gateway upstream error (redis failures fail the request,
/// not the process).
fn redis_err(e: &dyn std::fmt::Display) -> GatewayError {
    GatewayError::Upstream(format!("distributed gateway: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_orders_by_tier_then_fifo() {
        // Higher tier → higher score (dispatched first by BZPOPMAX).
        assert!(priority_score(5, 100) > priority_score(4, 0));
        assert!(priority_score(1, 0) > priority_score(0, 0));
        // Within a tier, earlier seq → higher score (FIFO).
        assert!(priority_score(3, 10) > priority_score(3, 11));
        assert!(priority_score(0, 1) > priority_score(0, 2));
    }

    #[test]
    fn score_is_exact_in_f64() {
        // Must round-trip through Redis's f64 score without precision loss.
        for (tier, seq) in [(0u8, 0u64), (255, 0), (255, SEQ_MASK), (7, 123_456_789)] {
            let s = priority_score(tier, seq);
            assert_eq!(s, s.trunc(), "score must be an exact integer f64");
            assert!(s.abs() < 2f64.powi(53), "score must fit in f64 mantissa");
        }
    }

    #[test]
    fn descriptor_and_result_round_trip() {
        let desc = JobDescriptor {
            id: "abc-1".into(),
            provider: "openai".into(),
            tier: 3,
            permits: 42,
            payload: serde_json::json!({"model": "gpt-5.5", "messages": []}),
        };
        let s = serde_json::to_string(&desc).unwrap();
        let back: JobDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "abc-1");
        assert_eq!(back.provider, "openai");
        assert_eq!(back.tier, 3);
        assert_eq!(back.permits, 42);

        let ok = serde_json::to_string(&JobResult::Ok(serde_json::json!({"a": 1}))).unwrap();
        assert!(matches!(
            serde_json::from_str::<JobResult>(&ok).unwrap(),
            JobResult::Ok(_)
        ));
        let err = serde_json::to_string(&JobResult::Err("boom".into())).unwrap();
        assert!(matches!(
            serde_json::from_str::<JobResult>(&err).unwrap(),
            JobResult::Err(m) if m == "boom"
        ));
    }

    // ---- integration (require a live Redis via LLMSHIM_REDIS_URL) -----------
    // Run: LLMSHIM_REDIS_URL=redis://127.0.0.1:6379 \
    //        cargo test --features gateway-redis -- --ignored

    use super::super::DispatchError;
    use crate::proxy::ratelimit::{InMemoryRateLimiter, RateLimitConfig};

    struct EchoDispatch;
    #[async_trait::async_trait]
    impl Dispatch for EchoDispatch {
        async fn dispatch(&self, _p: &str, payload: Value) -> Result<Value, DispatchError> {
            Ok(serde_json::json!({ "echo": payload }))
        }
    }

    fn unlimited() -> Arc<dyn RateLimiter> {
        Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default()))
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_submit_worker_bus_round_trip() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let gw = DistributedGateway::connect(
            &url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig::default(),
        )
        .await
        .expect("connect redis");
        gw.spawn_workers(vec!["itest-echo".to_string()]);

        // A request submitted here is popped by the worker and its result routed
        // back over the bus.
        let resp = gw
            .submit(GatewayRequest {
                provider: "itest-echo".to_string(),
                tier: 0,
                permits: 1,
                payload: serde_json::json!({ "hello": "world" }),
            })
            .await
            .expect("round trip");
        assert_eq!(resp["echo"]["hello"], "world");
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_zpopmax_orders_by_priority_then_fifo() {
        let Ok(url) = std::env::var("LLMSHIM_REDIS_URL") else {
            return;
        };
        let client = redis::Client::open(url).unwrap();
        let mut conn = ConnectionManager::new(client).await.unwrap();
        let key = "llmshim:gw:q:itest-prio";
        let _: () = conn.del(key).await.unwrap();

        // (member, tier, seq): tier 1 (seq 0, 1), tier 3 (seq 2).
        for (member, tier, seq) in [("a", 1u8, 0u64), ("b", 1, 1), ("c", 3, 2)] {
            let _: () = conn
                .zadd(key, member, priority_score(tier, seq))
                .await
                .unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            let popped: Vec<(String, f64)> = conn.zpopmax(key, 1).await.unwrap();
            got.push(popped[0].0.clone());
        }
        // tier 3 first, then tier 1 in FIFO (a before b).
        assert_eq!(got, vec!["c", "a", "b"]);
        let _: () = conn.del(key).await.unwrap();
    }
}
