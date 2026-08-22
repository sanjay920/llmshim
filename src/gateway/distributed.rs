//! Redis-backed **distributed gateway** (features `gateway` + `redis-coordination`).
//!
//! The in-memory [`Scheduler`](super::Scheduler) governs one process. A fleet of
//! gateway replicas (Cloud Run / ECS) needs a *shared* priority queue so any
//! instance can serve any request, ordered globally by tier then FIFO (with
//! aging), while the shared [`RedisRateLimiter`](crate::proxy::ratelimit) keeps
//! the whole fleet under one provider rate limit.
//!
//! As the advisor put it, you can't stretch the in-process `Job`/`RequestQueue`
//! (it holds a `oneshot`) across processes — so distributed mode is a **separate
//! seam**:
//!
//! * a **serializable priority queue** — a Redis sorted set per provider scored
//!   by a *virtual deadline* `enqueue_ms − tier·aging_step`, popped with
//!   `ZPOPMIN`. Higher tier and older age both yield an earlier deadline, so
//!   priority, FIFO, and **anti-starvation aging** all fall out of one static
//!   score with no re-scoring;
//! * an **at-least-once lease** — leasing atomically moves a job to a
//!   `processing` set with a visibility deadline (recording its score); a
//!   background **reaper** requeues leases whose deadline passed, so a worker
//!   that crashes mid-dispatch doesn't drop the request; and
//! * a **response bus** — Redis pub/sub on a per-request channel carrying typed
//!   [`BusMessage`]s, so the worker that dispatches a job streams the result
//!   (unary or chunked) back to the *origin* instance's open HTTP connection.
//!
//! ```text
//!   instance A: submit ─subscribe(resp:ID)─ZADD q:prov─┐            await bus
//!                                                       ▼
//!   shared Redis:   [ ZSET q:prov ] [ ZSET processing ] [ pub/sub resp:ID ]
//!                                                       ▲
//!   instance B: worker ─lease(ZPOPMIN→processing)─rate─dispatch─publish─ack
//!                                    reaper: expired processing → requeue
//! ```
//!
//! At-least-once means a redelivered job may run twice (idempotent upstream
//! calls, a wasted call at worst); streams refresh their lease as they run to
//! avoid mid-stream redelivery.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use super::{Dispatch, GatewayConfig, GatewayError, GatewayRequest, StreamChunk};
use crate::proxy::ratelimit::{RateKey, RateLimiter, RetryAfter};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Virtual-deadline score: **smaller = dispatched sooner** (`ZPOPMIN`). Higher
/// tier subtracts more, so it leads; and a smaller `enqueue_ms` (older job)
/// leads within a tier and eventually overtakes newer higher tiers — aging with
/// no re-scoring. Stays well within f64's exact-integer range.
fn deadline_score(tier: u8, enqueue_ms: u64, aging_step_ms: u64) -> f64 {
    enqueue_ms as f64 - (tier as u64 * aging_step_ms) as f64
}

fn queue_key(provider: &str) -> String {
    format!("llmshim:gw:q:{provider}")
}
fn processing_key(provider: &str) -> String {
    format!("llmshim:gw:proc:{provider}")
}
fn leased_key(provider: &str) -> String {
    format!("llmshim:gw:leased:{provider}")
}
fn response_channel(id: &str) -> String {
    format!("llmshim:gw:resp:{id}")
}

// Atomic lease: pop the earliest-deadline job, move it to `processing` with a
// visibility deadline, and record its score for redelivery. KEYS: queue,
// processing, leased. ARGV: visibility_deadline_ms.
const LEASE_LUA: &str = r#"
    local top = redis.call('ZPOPMIN', KEYS[1], 1)
    if #top == 0 then return false end
    local m = top[1]
    local s = top[2]
    redis.call('ZADD', KEYS[2], ARGV[1], m)
    redis.call('HSET', KEYS[3], m, s)
    return {m, s}
"#;

// Ack a completed job. KEYS: processing, leased. ARGV: member.
const ACK_LUA: &str = r#"
    redis.call('ZREM', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    return 1
"#;

// Release a lease back to the queue (e.g. rate-limited), keeping its score.
// KEYS: queue, processing, leased. ARGV: member, score.
const RELEASE_LUA: &str = r#"
    redis.call('ZADD', KEYS[1], ARGV[2], ARGV[1])
    redis.call('ZREM', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    return 1
"#;

// Reap expired leases back to the queue with their original score. KEYS:
// processing, queue, leased. ARGV: now_ms, limit.
const REAP_LUA: &str = r#"
    local expired = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', ARGV[1], 'LIMIT', 0, ARGV[2])
    local n = 0
    for _, m in ipairs(expired) do
        local s = redis.call('HGET', KEYS[3], m)
        if s then redis.call('ZADD', KEYS[2], s, m) end
        redis.call('ZREM', KEYS[1], m)
        redis.call('HDEL', KEYS[3], m)
        n = n + 1
    end
    return n
"#;

/// A queued unit of work, serialized into the Redis sorted set.
#[derive(Serialize, Deserialize)]
struct JobDescriptor {
    id: String,
    provider: String,
    tier: u8,
    permits: u32,
    payload: Value,
    #[serde(default)]
    stream: bool,
    /// Enqueue time (epoch ms) — drives the deadline score and the queue-wait
    /// metric.
    #[serde(default)]
    enqueue_ms: u64,
}

fn done_key(id: &str) -> String {
    format!("llmshim:gw:done:{id}")
}
fn attempts_key(id: &str) -> String {
    format!("llmshim:gw:attempts:{id}")
}
fn dlq_key(provider: &str) -> String {
    format!("llmshim:gw:dlq:{provider}")
}

/// A message on a request's response channel.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
enum BusMessage {
    /// Unary success.
    Unary(Value),
    /// A streaming chunk (raw provider SSE `data:` payload).
    Chunk(String),
    /// Stream complete.
    End,
    /// Failure (either mode).
    Error(String),
}

/// A Redis-backed distributed gateway. One per instance; runs the origin side
/// (`submit` / `submit_stream`) and the worker + reaper side.
pub struct DistributedGateway {
    client: redis::Client,
    conn: ConnectionManager,
    dispatch: Arc<dyn Dispatch>,
    limiter: Arc<dyn RateLimiter>,
    config: GatewayConfig,
    nonce: u128,
    counter: AtomicU64,
    lease: redis::Script,
    ack: redis::Script,
    release: redis::Script,
    reap: redis::Script,
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
            lease: redis::Script::new(LEASE_LUA),
            ack: redis::Script::new(ACK_LUA),
            release: redis::Script::new(RELEASE_LUA),
            reap: redis::Script::new(REAP_LUA),
        }))
    }

    /// Client-idempotency lookup: cached response for an `Idempotency-Key`.
    pub async fn idem_get(&self, key: &str) -> Option<Value> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn
            .get(format!("llmshim:gw:idem:{key}"))
            .await
            .unwrap_or(None);
        raw.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Cache a completed response under an `Idempotency-Key`.
    pub async fn idem_put(&self, key: &str, value: &Value, ttl_secs: u64) {
        if let Ok(s) = serde_json::to_string(value) {
            let mut conn = self.conn.clone();
            let _: Result<(), _> = conn
                .set_ex(format!("llmshim:gw:idem:{key}"), s, ttl_secs)
                .await;
        }
    }

    /// Liveness check: `PING` Redis (readiness gate for the fleet).
    pub async fn ping(&self) -> bool {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map(|r| r == "PONG")
            .unwrap_or(false)
    }

    /// Waiting-queue depth per provider (for metrics / introspection).
    pub async fn queue_depths(&self, providers: &[String]) -> Vec<(String, usize)> {
        let mut conn = self.conn.clone();
        let mut out = Vec::with_capacity(providers.len());
        for p in providers {
            let n: u64 = conn.zcard(queue_key(p)).await.unwrap_or(0);
            out.push((p.clone(), n as usize));
        }
        out
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", self.nonce, n)
    }

    fn aging_step_ms(&self) -> u64 {
        self.config.aging_step.as_millis() as u64
    }

    /// Enqueue a descriptor onto its provider's priority queue, first shedding
    /// with `Overloaded` if the waiting queue is at capacity.
    async fn enqueue(&self, desc: &JobDescriptor) -> Result<(), GatewayError> {
        let member = serde_json::to_string(desc).map_err(|e| redis_err(&e))?;
        let key = queue_key(&desc.provider);
        let mut conn = self.conn.clone();
        let depth: u64 = conn.zcard(&key).await.map_err(|e| redis_err(&e))?;
        if depth as usize >= self.config.max_queue_depth {
            return Err(GatewayError::Overloaded(self.config.overloaded_retry_after));
        }
        let score = deadline_score(desc.tier, desc.enqueue_ms, self.aging_step_ms());
        let _: () = conn
            .zadd(&key, &member, score)
            .await
            .map_err(|e| redis_err(&e))?;
        Ok(())
    }

    /// Origin side (unary): enqueue by priority and await the result over the bus.
    pub async fn submit(&self, req: GatewayRequest) -> Result<Value, GatewayError> {
        let desc = self.descriptor(&req, false);
        let channel = response_channel(&desc.id);

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
        self.enqueue(&desc).await?;

        use futures::StreamExt;
        let mut messages = pubsub.on_message();
        match tokio::time::timeout(self.config.request_timeout, messages.next()).await {
            Ok(Some(msg)) => {
                let payload: String = msg.get_payload().map_err(|e| redis_err(&e))?;
                match serde_json::from_str::<BusMessage>(&payload) {
                    Ok(BusMessage::Unary(v)) => Ok(v),
                    Ok(BusMessage::Error(e)) => Err(GatewayError::Upstream(e)),
                    Ok(_) => Err(GatewayError::Upstream("unexpected stream message".into())),
                    Err(e) => Err(GatewayError::Upstream(format!(
                        "bad response envelope: {e}"
                    ))),
                }
            }
            Ok(None) => Err(GatewayError::Shutdown),
            Err(_) => {
                self.remove_from_queue(&desc).await;
                Err(GatewayError::Timeout)
            }
        }
    }

    /// Origin side (streaming): enqueue by priority and return a channel of raw
    /// provider chunks routed back over the bus.
    pub async fn submit_stream(
        &self,
        req: GatewayRequest,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        let desc = self.descriptor(&req, true);
        let channel = response_channel(&desc.id);

        let mut pubsub = self
            .client
            .get_async_pubsub()
            .await
            .map_err(|e| redis_err(&e))?;
        pubsub
            .subscribe(&channel)
            .await
            .map_err(|e| redis_err(&e))?;
        // Depth-shed happens here so the caller can return 429/503 before SSE.
        self.enqueue(&desc).await?;

        let (chunk_tx, chunk_rx) = mpsc::channel(16);
        let request_timeout = self.config.request_timeout;
        let key = queue_key(&req.provider);
        let member = serde_json::to_string(&desc).map_err(|e| redis_err(&e))?;
        let conn = self.conn.clone();

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut messages = pubsub.on_message();
            let mut first = true;
            loop {
                match tokio::time::timeout(request_timeout, messages.next()).await {
                    Ok(Some(msg)) => {
                        first = false;
                        let payload: String = match msg.get_payload() {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        match serde_json::from_str::<BusMessage>(&payload) {
                            Ok(BusMessage::Chunk(s)) => {
                                if chunk_tx.send(Ok(s)).await.is_err() {
                                    break; // client disconnected
                                }
                            }
                            Ok(BusMessage::End) => break,
                            Ok(BusMessage::Error(e)) => {
                                let _ = chunk_tx.send(Err(GatewayError::Upstream(e))).await;
                                break;
                            }
                            Ok(BusMessage::Unary(_)) | Err(_) => break,
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        // Never dispatched → drop it from the queue so no worker
                        // burns a token on an abandoned request.
                        if first {
                            let mut c = conn.clone();
                            let _: Result<i64, _> = c.zrem(&key, &member).await;
                        }
                        let _ = chunk_tx.send(Err(GatewayError::Timeout)).await;
                        break;
                    }
                }
            }
        });

        Ok(chunk_rx)
    }

    fn descriptor(&self, req: &GatewayRequest, stream: bool) -> JobDescriptor {
        JobDescriptor {
            id: self.next_id(),
            provider: req.provider.clone(),
            tier: req.tier,
            permits: req.permits.max(1),
            payload: req.payload.clone(),
            stream,
            enqueue_ms: now_ms(),
        }
    }

    /// TTL for the done / attempts markers — a few lease windows, long enough to
    /// outlast redelivery but short enough not to leak keys.
    fn marker_ttl_secs(&self) -> u64 {
        (self.config.lease_timeout.as_secs() * 3).max(60)
    }

    async fn is_done(&self, id: &str) -> bool {
        let mut conn = self.conn.clone();
        conn.exists(done_key(id)).await.unwrap_or(false)
    }

    async fn mark_done(&self, id: &str) {
        let mut conn = self.conn.clone();
        let _: Result<(), _> = conn.set_ex(done_key(id), 1, self.marker_ttl_secs()).await;
    }

    /// Increment and return this job's delivery-attempt count.
    async fn bump_attempts(&self, id: &str) -> u32 {
        let mut conn = self.conn.clone();
        let key = attempts_key(id);
        let n: u64 = conn.incr(&key, 1).await.unwrap_or(1);
        let _: Result<bool, _> = conn.expire(&key, self.marker_ttl_secs() as i64).await;
        n as u32
    }

    async fn dead_letter(&self, provider: &str, member: &str) {
        let mut conn = self.conn.clone();
        let _: Result<i64, _> = conn.lpush(dlq_key(provider), member).await;
    }

    /// Number of dead-lettered jobs for a provider (introspection).
    pub async fn dead_letter_len(&self, provider: &str) -> usize {
        let mut conn = self.conn.clone();
        let n: u64 = conn.llen(dlq_key(provider)).await.unwrap_or(0);
        n as usize
    }

    async fn remove_from_queue(&self, desc: &JobDescriptor) {
        if let Ok(member) = serde_json::to_string(desc) {
            let mut conn = self.conn.clone();
            let _: Result<i64, _> = conn.zrem(queue_key(&desc.provider), member).await;
        }
    }

    /// Spawn one worker loop per provider plus a reaper for redelivery.
    pub fn spawn_workers(self: &Arc<Self>, providers: Vec<String>) -> Vec<JoinHandle<()>> {
        let mut handles: Vec<JoinHandle<()>> = providers
            .iter()
            .cloned()
            .map(|provider| {
                let me = self.clone();
                tokio::spawn(async move { me.worker(provider).await })
            })
            .collect();
        let me = self.clone();
        handles.push(tokio::spawn(async move { me.reaper(providers).await }));
        handles
    }

    async fn worker(self: Arc<Self>, provider: String) {
        const IDLE_POLL: Duration = Duration::from_millis(50);
        let qkey = queue_key(&provider);
        let pkey = processing_key(&provider);
        let lkey = leased_key(&provider);
        let rate_key = RateKey::provider(provider.clone());
        let sem = Arc::new(Semaphore::new(
            self.config.max_concurrency_per_provider.max(1),
        ));
        let mut conn = self.conn.clone();

        loop {
            let deadline = now_ms() + self.config.lease_timeout.as_millis() as u64;
            let leased: Option<(String, String)> = match self
                .lease
                .key(&qkey)
                .key(&pkey)
                .key(&lkey)
                .arg(deadline)
                .invoke_async(&mut conn)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("gateway worker[{provider}]: lease error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let Some((member, score)) = leased else {
                tokio::time::sleep(IDLE_POLL).await; // queue empty
                continue;
            };
            let desc: JobDescriptor = match serde_json::from_str(&member) {
                Ok(d) => d,
                Err(_) => {
                    self.ack_lease(&provider, &member).await; // drop a corrupt entry
                    continue;
                }
            };

            // Idempotency: a job that already completed (then got redelivered by
            // the reaper) is skipped.
            if self.is_done(&desc.id).await {
                self.ack_lease(&provider, &member).await;
                continue;
            }
            // Poison-job guard: dead-letter after too many delivery attempts.
            let attempts = self.bump_attempts(&desc.id).await;
            if attempts > self.config.max_attempts {
                eprintln!(
                    "gateway worker[{provider}]: dead-lettering job {} after {attempts} attempts",
                    desc.id
                );
                self.dead_letter(&provider, &member).await;
                self.ack_lease(&provider, &member).await;
                crate::gateway::metrics::incr(
                    crate::gateway::metrics::REJECTED,
                    &[("provider", &provider), ("reason", "dead_letter")],
                );
                continue;
            }

            match self.limiter.acquire(&rate_key, desc.permits).await {
                Ok(()) => {
                    let permit = match sem.clone().acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => break,
                    };
                    let me = self.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        me.run_and_publish(desc, member).await;
                    });
                }
                Err(RetryAfter(wait)) => {
                    // Release the lease back to the queue (priority preserved).
                    let _: Result<i64, _> = self
                        .release
                        .key(&qkey)
                        .key(&pkey)
                        .key(&lkey)
                        .arg(&member)
                        .arg(&score)
                        .invoke_async(&mut conn)
                        .await;
                    tokio::time::sleep(wait.min(Duration::from_millis(500))).await;
                }
            }
        }
    }

    /// Dispatch a leased job, publish result(s) to its channel, then mark it done
    /// (idempotency) and ack the lease.
    async fn run_and_publish(&self, desc: JobDescriptor, member: String) {
        use crate::gateway::metrics;
        let channel = response_channel(&desc.id);
        let provider = desc.provider.clone();
        let plabels: &[(&str, &str)] = &[("provider", &provider)];
        let _inflight = metrics::inflight(&provider);
        metrics::observe_ms(
            metrics::QUEUE_WAIT,
            plabels,
            now_ms().saturating_sub(desc.enqueue_ms) as f64,
        );
        let started_at = now_ms();

        if desc.stream {
            match self.dispatch.dispatch_stream(&provider, desc.payload).await {
                Ok(mut upstream) => {
                    metrics::incr(metrics::DISPATCHED, plabels);
                    use futures::StreamExt;
                    let refresh_every = (self.config.lease_timeout / 3).max(Duration::from_secs(1));
                    let mut next_refresh = now_ms() + refresh_every.as_millis() as u64;
                    while let Some(item) = upstream.next().await {
                        let (msg, stop) = match item {
                            Ok(chunk) => (BusMessage::Chunk(chunk), false),
                            Err(e) => (BusMessage::Error(e.to_string()), true),
                        };
                        self.publish(&channel, &msg).await;
                        if stop {
                            break;
                        }
                        // Keep the lease alive so the reaper doesn't redeliver a
                        // long-running stream mid-flight.
                        if now_ms() >= next_refresh {
                            self.refresh_lease(&provider, &member).await;
                            next_refresh = now_ms() + refresh_every.as_millis() as u64;
                        }
                    }
                    self.publish(&channel, &BusMessage::End).await;
                    metrics::observe_ms(
                        metrics::UPSTREAM_LATENCY,
                        plabels,
                        now_ms().saturating_sub(started_at) as f64,
                    );
                }
                Err(err) => {
                    metrics::incr(
                        metrics::REJECTED,
                        &[("provider", &provider), ("reason", "upstream")],
                    );
                    self.penalize_if_429(&provider, &err).await;
                    self.publish(&channel, &BusMessage::Error(err.message))
                        .await;
                }
            }
        } else {
            let msg = match self.dispatch.dispatch(&provider, desc.payload).await {
                Ok(value) => {
                    metrics::incr(metrics::DISPATCHED, plabels);
                    metrics::observe_ms(
                        metrics::UPSTREAM_LATENCY,
                        plabels,
                        now_ms().saturating_sub(started_at) as f64,
                    );
                    BusMessage::Unary(value)
                }
                Err(err) => {
                    metrics::incr(
                        metrics::REJECTED,
                        &[("provider", &provider), ("reason", "upstream")],
                    );
                    self.penalize_if_429(&provider, &err).await;
                    BusMessage::Error(err.message)
                }
            };
            self.publish(&channel, &msg).await;
        }

        // Idempotency marker so a late redelivery of this (now-complete) job is
        // skipped, then release the lease.
        self.mark_done(&desc.id).await;
        self.ack_lease(&provider, &member).await;
    }

    async fn publish(&self, channel: &str, msg: &BusMessage) {
        if let Ok(payload) = serde_json::to_string(msg) {
            let mut conn = self.conn.clone();
            let _: Result<i64, _> = conn.publish(channel, payload).await;
        }
    }

    async fn ack_lease(&self, provider: &str, member: &str) {
        let mut conn = self.conn.clone();
        let _: Result<i64, _> = self
            .ack
            .key(processing_key(provider))
            .key(leased_key(provider))
            .arg(member)
            .invoke_async(&mut conn)
            .await;
    }

    async fn refresh_lease(&self, provider: &str, member: &str) {
        let deadline = now_ms() + self.config.lease_timeout.as_millis() as u64;
        let mut conn = self.conn.clone();
        // XX: only refresh if still leased (not acked/reaped).
        let _: Result<i64, _> = redis::cmd("ZADD")
            .arg(processing_key(provider))
            .arg("XX")
            .arg(deadline)
            .arg(member)
            .query_async(&mut conn)
            .await;
    }

    async fn penalize_if_429(&self, provider: &str, err: &super::DispatchError) {
        if let Some(retry_after) = err.retry_after {
            self.limiter
                .penalize(&RateKey::provider(provider.to_string()), retry_after)
                .await;
        }
    }

    /// Run the reaper loop: periodically requeue leases whose visibility deadline
    /// has passed (a crashed or stuck worker).
    async fn reaper(self: Arc<Self>, providers: Vec<String>) {
        let interval = (self.config.lease_timeout / 3).max(Duration::from_secs(1));
        loop {
            tokio::time::sleep(interval).await;
            for provider in &providers {
                let reaped = self.reap_once(provider).await;
                if reaped > 0 {
                    eprintln!("gateway reaper[{provider}]: redelivered {reaped} expired lease(s)");
                }
            }
        }
    }

    /// Requeue any leases for `provider` whose visibility deadline has passed.
    /// Returns how many were redelivered. Public so a fleet can also drive
    /// reaping from an external scheduler.
    pub async fn reap_once(&self, provider: &str) -> i64 {
        let mut conn = self.conn.clone();
        self.reap
            .key(processing_key(provider))
            .key(queue_key(provider))
            .key(leased_key(provider))
            .arg(now_ms())
            .arg(256)
            .invoke_async(&mut conn)
            .await
            .unwrap_or(0)
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

    const STEP: u64 = 5000; // aging_step ms

    #[test]
    fn deadline_orders_by_tier_then_age() {
        // Higher tier → smaller (earlier) deadline → dispatched first (ZPOPMIN).
        assert!(deadline_score(5, 1000, STEP) < deadline_score(4, 1000, STEP));
        assert!(deadline_score(1, 1000, STEP) < deadline_score(0, 1000, STEP));
        // Within a tier, older (smaller enqueue_ms) → smaller deadline (FIFO).
        assert!(deadline_score(3, 1000, STEP) < deadline_score(3, 2000, STEP));
    }

    #[test]
    fn aging_lets_old_low_tier_overtake_new_high_tier() {
        // A tier-0 job enqueued long enough before a tier-5 job wins: its
        // deadline (1_000_000) is earlier than the tier-5 deadline
        // (1_030_000 - 5*5000 = 1_005_000).
        let old_low = deadline_score(0, 1_000_000, STEP);
        let new_high = deadline_score(5, 1_030_000, STEP);
        assert!(
            old_low < new_high,
            "aged low tier should overtake fresh high tier"
        );
        // But a *recent* low tier does not.
        let recent_low = deadline_score(0, 1_029_000, STEP);
        assert!(recent_low > new_high);
    }

    #[test]
    fn deadline_is_exact_in_f64() {
        for (tier, ms) in [
            (0u8, 0u64),
            (255, 2_000_000_000_000),
            (7, 1_700_000_000_000),
        ] {
            let s = deadline_score(tier, ms, STEP);
            assert_eq!(s, s.trunc(), "score must be an exact integer f64");
            assert!(s.abs() < 2f64.powi(53));
        }
    }

    #[test]
    fn descriptor_and_bus_messages_round_trip() {
        let desc = JobDescriptor {
            id: "abc-1".into(),
            provider: "openai".into(),
            tier: 3,
            permits: 42,
            payload: serde_json::json!({"model": "gpt-5.5"}),
            stream: true,
            enqueue_ms: 1_700_000_000_000,
        };
        let back: JobDescriptor =
            serde_json::from_str(&serde_json::to_string(&desc).unwrap()).unwrap();
        assert_eq!(back.id, "abc-1");
        assert!(back.stream);

        for msg in [
            BusMessage::Unary(serde_json::json!({"a": 1})),
            BusMessage::Chunk("hello".into()),
            BusMessage::End,
            BusMessage::Error("boom".into()),
        ] {
            let s = serde_json::to_string(&msg).unwrap();
            let _back: BusMessage = serde_json::from_str(&s).unwrap();
        }
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
        async fn dispatch_stream(
            &self,
            _p: &str,
            _payload: Value,
        ) -> Result<super::super::ChunkStream, DispatchError> {
            let chunks: Vec<StreamChunk> = vec![Ok("a".into()), Ok("b".into()), Ok("c".into())];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    fn unlimited() -> Arc<dyn RateLimiter> {
        Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default()))
    }

    async fn test_gateway(provider_seed: &str) -> Option<Arc<DistributedGateway>> {
        let url = std::env::var("LLMSHIM_REDIS_URL").ok()?;
        // Clean the keys this test uses so reruns are deterministic.
        let client = redis::Client::open(url.clone()).unwrap();
        let mut conn = ConnectionManager::new(client).await.unwrap();
        for k in [
            queue_key(provider_seed),
            processing_key(provider_seed),
            leased_key(provider_seed),
        ] {
            let _: Result<i64, _> = conn.del(k).await;
        }
        DistributedGateway::connect(
            &url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig::default(),
        )
        .await
        .ok()
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_unary_round_trip() {
        let Some(gw) = test_gateway("itest-unary").await else {
            return;
        };
        gw.spawn_workers(vec!["itest-unary".into()]);
        let resp = gw
            .submit(GatewayRequest {
                provider: "itest-unary".into(),
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
    async fn redis_stream_round_trip() {
        let Some(gw) = test_gateway("itest-stream").await else {
            return;
        };
        gw.spawn_workers(vec!["itest-stream".into()]);
        let mut rx = gw
            .submit_stream(GatewayRequest {
                provider: "itest-stream".into(),
                tier: 0,
                permits: 1,
                payload: serde_json::json!({}),
            })
            .await
            .expect("stream start");
        let mut got = Vec::new();
        while let Some(item) = rx.recv().await {
            got.push(item.expect("chunk"));
        }
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_reaper_redelivers_expired_lease() {
        let provider = "itest-reap";
        let Some(gw) = test_gateway(provider).await else {
            return;
        };
        let mut conn = gw.conn.clone();
        // Simulate a crashed worker: a job sits in `processing` with a deadline
        // already in the past, and its score is recorded in `leased`.
        let member = r#"{"id":"x","provider":"itest-reap","tier":0,"permits":1,"payload":{},"stream":false}"#;
        let orig_score = deadline_score(0, 1_700_000_000_000, STEP);
        let _: () = conn
            .zadd(processing_key(provider), member, 1u64)
            .await
            .unwrap();
        let _: () = conn
            .hset(leased_key(provider), member, orig_score)
            .await
            .unwrap();

        let reaped = gw.reap_once(provider).await;
        assert_eq!(reaped, 1, "expired lease should be redelivered");
        // It's back on the queue with its original score, and gone from processing.
        let qlen: u64 = conn.zcard(queue_key(provider)).await.unwrap();
        let plen: u64 = conn.zcard(processing_key(provider)).await.unwrap();
        assert_eq!(qlen, 1);
        assert_eq!(plen, 0);
        let score: f64 = conn.zscore(queue_key(provider), member).await.unwrap();
        assert_eq!(score, orig_score);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_dedup_and_dead_letter_markers() {
        let provider = "itest-dedup";
        let Some(gw) = test_gateway(provider).await else {
            return;
        };
        let mut conn = gw.conn.clone();
        for k in [done_key("job-x"), attempts_key("job-y"), dlq_key(provider)] {
            let _: Result<i64, _> = conn.del(k).await;
        }

        // Idempotency marker.
        assert!(!gw.is_done("job-x").await);
        gw.mark_done("job-x").await;
        assert!(gw.is_done("job-x").await);

        // Attempt counter increments per delivery.
        assert_eq!(gw.bump_attempts("job-y").await, 1);
        assert_eq!(gw.bump_attempts("job-y").await, 2);

        // Dead-letter queue.
        assert_eq!(gw.dead_letter_len(provider).await, 0);
        gw.dead_letter(provider, "poison-member").await;
        assert_eq!(gw.dead_letter_len(provider).await, 1);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_zpopmin_orders_by_tier_then_age() {
        let provider = "itest-order";
        let Some(gw) = test_gateway(provider).await else {
            return;
        };
        let mut conn = gw.conn.clone();
        let key = queue_key(provider);
        // tier 1 (older, newer), tier 3.
        for (member, tier, ms) in [("a", 1u8, 1000u64), ("b", 1, 2000), ("c", 3, 3000)] {
            let _: () = conn
                .zadd(&key, member, deadline_score(tier, ms, STEP))
                .await
                .unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            let popped: Vec<(String, f64)> = conn.zpopmin(&key, 1).await.unwrap();
            got.push(popped[0].0.clone());
        }
        // tier 3 first, then tier 1 in FIFO (a before b).
        assert_eq!(got, vec!["c", "a", "b"]);
    }
}
