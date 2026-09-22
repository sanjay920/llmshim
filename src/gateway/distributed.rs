//! Redis-backed **distributed gateway** (features `gateway` + `redis-coordination`).
//!
//! The in-memory [`Scheduler`](super::Scheduler) governs one process. A fleet of
//! gateway replicas (Cloud Run / ECS) needs a *shared* priority queue so any
//! instance can serve any request, ordered globally by tier then FIFO (with
//! aging). Built-in HTTP jobs carry a server-created policy scope so the shared
//! coordinator atomically gates provider and tenant limits at each real send;
//! unscoped custom jobs retain the standalone Redis limiter behavior.
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

mod admission;

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
fn scoped_idempotency_key(context: &crate::gateway::idempotency::IdempotencyContext) -> String {
    format!("llmshim:gw:idem:v2:{}", context.storage_key())
}
fn generic_idempotency_key(client_key: &str) -> String {
    format!(
        "llmshim:gw:idem:generic:v1:{}",
        crate::gateway::idempotency::generic_storage_key(client_key)
    )
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
    policy_envelope_version: u8,
    #[serde(default)]
    trusted_unscoped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy_scope: Option<crate::gateway::attempt::TrustedPolicyScope>,
    #[serde(default)]
    stream: bool,
    /// Enqueue time (epoch ms) — drives the deadline score and the queue-wait
    /// metric.
    #[serde(default)]
    enqueue_ms: u64,
}

pub(crate) struct PreparedSubmission {
    descriptor: JobDescriptor,
    member: String,
}

pub(crate) struct AcceptedSubmission {
    prepared: PreparedSubmission,
    pubsub: redis::aio::PubSub,
}

impl PreparedSubmission {
    fn new(descriptor: JobDescriptor) -> Result<Self, GatewayError> {
        let member = serde_json::to_string(&descriptor).map_err(|error| redis_err(&error))?;
        Ok(Self { descriptor, member })
    }
}

fn policy_envelope_is_current(descriptor: &JobDescriptor) -> bool {
    descriptor.policy_envelope_version == 1
        && (descriptor.policy_scope.is_some() || descriptor.trusted_unscoped)
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
    attempt_coordinator: Option<Arc<crate::gateway::attempt::AttemptCoordinator>>,
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
        Self::connect_inner(redis_url, dispatch, limiter, config, None).await
    }

    pub(crate) async fn connect_with_coordinator(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
        attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    ) -> redis::RedisResult<Arc<Self>> {
        Self::connect_inner(
            redis_url,
            dispatch,
            limiter,
            config,
            Some(attempt_coordinator),
        )
        .await
    }

    async fn connect_inner(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
        attempt_coordinator: Option<Arc<crate::gateway::attempt::AttemptCoordinator>>,
    ) -> redis::RedisResult<Arc<Self>> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client.clone()).await?;
        let nonce = uuid::Uuid::new_v4().as_u128();
        Ok(Arc::new(Self {
            client,
            conn,
            dispatch,
            limiter,
            attempt_coordinator,
            config,
            nonce,
            counter: AtomicU64::new(0),
            lease: redis::Script::new(LEASE_LUA),
            ack: redis::Script::new(ACK_LUA),
            release: redis::Script::new(RELEASE_LUA),
            reap: redis::Script::new(REAP_LUA),
        }))
    }

    /// Generic Redis cache lookup retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub async fn idem_get(&self, key: &str) -> Option<Value> {
        let mut connection = self.conn.clone();
        let serialized_value: Option<String> = connection
            .get(generic_idempotency_key(key))
            .await
            .unwrap_or(None);
        serialized_value.and_then(|value| serde_json::from_str(&value).ok())
    }

    /// Generic Redis cache insertion retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub async fn idem_put(&self, key: &str, value: &Value, ttl_secs: u64) {
        if let Ok(serialized_value) = serde_json::to_string(value) {
            let mut connection = self.conn.clone();
            let _: Result<(), _> = connection
                .set_ex(generic_idempotency_key(key), serialized_value, ttl_secs)
                .await;
        }
    }

    /// HTTP-gateway lookup for one credential-, route-, and request-bound key.
    pub(crate) async fn scoped_idem_lookup(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
    ) -> crate::gateway::idempotency::IdempotencyLookup {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn
            .get(scoped_idempotency_key(context))
            .await
            .unwrap_or(None);
        raw.and_then(|serialized| serde_json::from_str(&serialized).ok())
            .map_or(
                crate::gateway::idempotency::IdempotencyLookup::Miss,
                |cached_response: crate::gateway::idempotency::CachedResponse| {
                    cached_response.lookup(context)
                },
            )
    }

    /// Store an HTTP-gateway response under its request-bound context.
    pub(crate) async fn scoped_idem_store(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
        value: &Value,
        ttl_secs: u64,
    ) {
        let cached_response =
            crate::gateway::idempotency::CachedResponse::new(context, value.clone());
        if let Ok(serialized_response) = serde_json::to_string(&cached_response) {
            let mut conn = self.conn.clone();
            let _: Result<(), _> = conn
                .set_ex(
                    scoped_idempotency_key(context),
                    serialized_response,
                    ttl_secs,
                )
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
    async fn enqueue(&self, prepared_submission: &PreparedSubmission) -> Result<(), GatewayError> {
        let descriptor = &prepared_submission.descriptor;
        let provider_queue_key = queue_key(&descriptor.provider);
        let mut connection = self.conn.clone();
        let priority_score =
            deadline_score(descriptor.tier, descriptor.enqueue_ms, self.aging_step_ms());
        let admitted = admission::enqueue(
            &mut connection,
            &provider_queue_key,
            self.config.max_queue_depth,
            priority_score,
            &prepared_submission.member,
        )
        .await
        .map_err(|error| redis_err(&error))?;
        if !admitted {
            return Err(GatewayError::Overloaded(self.config.overloaded_retry_after));
        }
        Ok(())
    }

    /// Origin side (unary): enqueue by priority and await the result over the bus.
    pub async fn submit(&self, req: GatewayRequest) -> Result<Value, GatewayError> {
        let prepared = self.prepare_submission(req, false, None)?;
        self.submit_prepared(prepared).await
    }

    pub(crate) fn prepare_with_policy(
        &self,
        req: GatewayRequest,
        stream: bool,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    ) -> Result<PreparedSubmission, GatewayError> {
        self.prepare_submission(req, stream, Some(policy_scope))
    }

    pub(crate) async fn submit_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<Value, GatewayError> {
        let accepted = self.accept_prepared(prepared).await?;
        self.await_accepted(accepted).await
    }

    pub(crate) async fn accept_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<AcceptedSubmission, GatewayError> {
        let channel = response_channel(&prepared.descriptor.id);

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
        self.enqueue(&prepared).await?;
        Ok(AcceptedSubmission { prepared, pubsub })
    }

    pub(crate) async fn await_accepted(
        &self,
        mut accepted: AcceptedSubmission,
    ) -> Result<Value, GatewayError> {
        use futures::StreamExt;
        let mut messages = accepted.pubsub.on_message();
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
                self.remove_from_queue(&accepted.prepared).await;
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
        let prepared = self.prepare_submission(req, true, None)?;
        self.submit_stream_prepared(prepared).await
    }

    pub(crate) async fn submit_stream_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        let accepted = self.accept_prepared(prepared).await?;
        Ok(self.stream_accepted(accepted))
    }

    pub(crate) fn stream_accepted(
        &self,
        mut accepted: AcceptedSubmission,
    ) -> mpsc::Receiver<StreamChunk> {
        let (chunk_tx, chunk_rx) = mpsc::channel(16);
        let request_timeout = self.config.request_timeout;
        let key = queue_key(&accepted.prepared.descriptor.provider);
        let member = accepted.prepared.member;
        let conn = self.conn.clone();

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut messages = accepted.pubsub.on_message();
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

        chunk_rx
    }

    fn prepare_submission(
        &self,
        req: GatewayRequest,
        stream: bool,
        policy_scope: Option<crate::gateway::attempt::TrustedPolicyScope>,
    ) -> Result<PreparedSubmission, GatewayError> {
        let descriptor = JobDescriptor {
            id: self.next_id(),
            provider: req.provider,
            tier: req.tier,
            permits: req.permits.max(1),
            payload: req.payload,
            policy_envelope_version: 1,
            trusted_unscoped: policy_scope.is_none(),
            policy_scope,
            stream,
            enqueue_ms: now_ms(),
        };
        PreparedSubmission::new(descriptor)
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

    async fn remove_from_queue(&self, prepared: &PreparedSubmission) {
        let mut conn = self.conn.clone();
        let _: Result<i64, _> = conn
            .zrem(queue_key(&prepared.descriptor.provider), &prepared.member)
            .await;
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
            let preparation_permit = match sem.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
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
                    drop(preparation_permit);
                    eprintln!("gateway worker[{provider}]: lease error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let Some((member, score)) = leased else {
                drop(preparation_permit);
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

            if !policy_envelope_is_current(&desc) {
                self.publish(
                    &response_channel(&desc.id),
                    &BusMessage::Error("llmshim-coordinator-unavailable".into()),
                )
                .await;
                self.mark_done(&desc.id).await;
                self.ack_lease(&provider, &member).await;
                continue;
            }

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

            let policy_gated = desc.policy_scope.is_some();
            let rate_admission = if policy_gated {
                Ok(())
            } else {
                self.limiter.acquire(&rate_key, desc.permits).await
            };
            match rate_admission {
                Ok(()) => {
                    let me = self.clone();
                    tokio::spawn(async move {
                        let _preparation_permit = preparation_permit;
                        me.run_and_publish(desc, member).await;
                    });
                }
                Err(RetryAfter(wait)) => {
                    drop(preparation_permit);
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

        let policy_context = match (&desc.policy_scope, &self.attempt_coordinator) {
            (Some(scope), Some(coordinator)) => Some(coordinator.context(scope.clone())),
            (Some(_), None) => {
                self.publish(
                    &channel,
                    &BusMessage::Error("llmshim-coordinator-unavailable".into()),
                )
                .await;
                self.ack_lease(&provider, &member).await;
                return;
            }
            (None, _) => None,
        };

        let policy_gated = desc.policy_scope.is_some();
        if desc.stream {
            let opened = match policy_context {
                Some(context) => {
                    self.dispatch
                        .dispatch_stream_with_policy(&provider, desc.payload, context)
                        .await
                }
                None => self.dispatch.dispatch_stream(&provider, desc.payload).await,
            };
            match opened {
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
                    if !policy_gated {
                        self.penalize_if_429(&provider, &err).await;
                    }
                    self.publish(&channel, &BusMessage::Error(err.message))
                        .await;
                }
            }
        } else {
            let dispatched = match policy_context {
                Some(context) => {
                    self.dispatch
                        .dispatch_with_policy(&provider, desc.payload, context)
                        .await
                }
                None => self.dispatch.dispatch(&provider, desc.payload).await,
            };
            let msg = match dispatched {
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
                    if !policy_gated {
                        self.penalize_if_429(&provider, &err).await;
                    }
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

/// Fleet-wide spend ledger: one counter per `(tenant, window)` in the same
/// Redis the queue uses, so a dollar cap is global rather than per replica.
/// A Redis failure loses the charge rather than the request — the same
/// fail-open posture the shared rate limiter takes.
#[async_trait::async_trait]
impl crate::gateway::quota::SpendStore for DistributedGateway {
    async fn spent(&self, tenant: &str, window: Duration) -> f64 {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(spend_key(tenant, window)).await.unwrap_or(None);
        raw.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
    }

    async fn record(&self, tenant: &str, window: Duration, usd: f64) {
        let key = spend_key(tenant, window);
        let mut conn = self.conn.clone();
        let _: Result<f64, _> = conn.incr(&key, usd).await;
        // Two windows of slack so a late charge still lands on its own window.
        let ttl = window.as_millis().saturating_mul(2).min(u64::MAX as u128) as u64;
        let _: Result<(), _> = conn.pexpire(&key, ttl as i64).await;
    }
}

fn spend_key(tenant: &str, window: Duration) -> String {
    format!(
        "llmshim:spend:{tenant}:{}",
        crate::gateway::quota::window_index(window)
    )
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
        let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(
            &crate::gateway::auth::Identity {
                tenant: "server-owned".into(),
                tier: 3,
                rpm: Some(1),
                tpm: Some(42),
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        );
        let desc = JobDescriptor {
            id: "abc-1".into(),
            provider: "openai".into(),
            tier: 3,
            permits: 42,
            payload: serde_json::json!({"model": "gpt-5.5"}),
            policy_envelope_version: 1,
            trusted_unscoped: false,
            policy_scope: Some(policy_scope),
            stream: true,
            enqueue_ms: 1_700_000_000_000,
        };
        let back: JobDescriptor =
            serde_json::from_str(&serde_json::to_string(&desc).unwrap()).unwrap();
        assert_eq!(back.id, "abc-1");
        assert!(back.stream);
        assert!(back.policy_scope.is_some());
        let serialized = serde_json::to_string(&desc).unwrap();
        assert!(!serialized.contains("server-owned"));
        let prepared = PreparedSubmission::new(desc).unwrap();
        assert_eq!(prepared.member, serialized);
        let prepared_descriptor: JobDescriptor = serde_json::from_str(&prepared.member).unwrap();
        assert_eq!(prepared_descriptor.id, "abc-1");
        assert!(prepared_descriptor.stream);

        let legacy: JobDescriptor = serde_json::from_value(serde_json::json!({
            "id": "old",
            "provider": "openai",
            "tier": 1,
            "permits": 1,
            "payload": {"model": "gpt-5.5"}
        }))
        .unwrap();
        assert!(!policy_envelope_is_current(&legacy));

        let trusted_unscoped = JobDescriptor {
            id: "custom".into(),
            provider: "custom".into(),
            tier: 0,
            permits: 1,
            payload: serde_json::json!({}),
            policy_envelope_version: 1,
            trusted_unscoped: true,
            policy_scope: None,
            stream: false,
            enqueue_ms: 0,
        };
        assert!(policy_envelope_is_current(&trusted_unscoped));

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

    struct LatchBlockedDispatch {
        dispatch_starts: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_started: Arc<tokio::sync::Notify>,
        release_dispatches: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl Dispatch for LatchBlockedDispatch {
        async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
            self.dispatch_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.dispatch_started.notify_one();
            self.release_dispatches
                .acquire()
                .await
                .expect("test release semaphore remains open")
                .forget();
            Ok(payload)
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
    async fn redis_concurrent_origins_share_one_remaining_queue_slot() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider_name = format!("queue-admission-{}", uuid::Uuid::new_v4());
        let mut gateways = Vec::new();
        for _ in 0..3 {
            gateways.push(
                DistributedGateway::connect(
                    &redis_url,
                    Arc::new(EchoDispatch),
                    unlimited(),
                    GatewayConfig {
                        max_queue_depth: 2,
                        ..GatewayConfig::default()
                    },
                )
                .await
                .unwrap(),
            );
        }
        let prepare_submission = |request_id: &str, tier: u8| {
            PreparedSubmission::new(JobDescriptor {
                id: request_id.to_owned(),
                provider: provider_name.clone(),
                tier,
                permits: 1,
                payload: serde_json::json!({"request": request_id}),
                policy_scope: None,
                stream: false,
                enqueue_ms: 1_700_000_000_000,
            })
            .unwrap()
        };
        let existing_submission = prepare_submission("existing", 0);
        gateways[0].enqueue(&existing_submission).await.unwrap();
        let prepared_submissions = [
            prepare_submission("origin-a", 1),
            prepare_submission("origin-b", 2),
            prepare_submission("origin-c", 3),
        ];
        let results = futures::future::join_all(
            gateways
                .iter()
                .zip(prepared_submissions.iter())
                .map(|(gateway, prepared)| gateway.enqueue(prepared)),
        )
        .await;
        let mut connection = gateways[0].conn.clone();
        let queued_members: Vec<String> = connection
            .zrange(queue_key(&provider_name), 0, -1)
            .await
            .unwrap();
        let _: i64 = connection.del(queue_key(&provider_name)).await.unwrap();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(queued_members.len(), 2);
        for (result, prepared) in results.iter().zip(prepared_submissions.iter()) {
            if result.is_ok() {
                assert_eq!(queued_members[0], prepared.member);
            } else {
                assert!(matches!(result, Err(GatewayError::Overloaded(_))));
                assert!(!queued_members.contains(&prepared.member));
            }
        }
        assert_eq!(queued_members[1], existing_submission.member);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_queue_admission_zero_capacity_and_storage_errors_fail_closed() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider_name = format!("queue-failure-{}", uuid::Uuid::new_v4());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig {
                max_queue_depth: 0,
                overloaded_retry_after: Duration::from_secs(7),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let prepared_submission = PreparedSubmission::new(JobDescriptor {
            id: "refused".into(),
            provider: provider_name.clone(),
            tier: 0,
            permits: 1,
            payload: serde_json::json!({"request": "synthetic payload"}),
            policy_scope: None,
            stream: false,
            enqueue_ms: 1_700_000_000_000,
        })
        .unwrap();
        assert!(matches!(
            gateway.enqueue(&prepared_submission).await,
            Err(GatewayError::Overloaded(delay)) if delay == Duration::from_secs(7)
        ));
        let provider_queue_key = queue_key(&provider_name);
        let mut connection = gateway.conn.clone();
        let queue_exists: bool = connection.exists(&provider_queue_key).await.unwrap();
        assert!(!queue_exists);

        let _: () = connection
            .set(&provider_queue_key, "existing value")
            .await
            .unwrap();
        assert!(matches!(
            gateway.enqueue(&prepared_submission).await,
            Err(GatewayError::Upstream(_))
        ));
        let stored_value: String = connection.get(&provider_queue_key).await.unwrap();
        assert_eq!(stored_value, "existing value");
        let _: i64 = connection.del(provider_queue_key).await.unwrap();
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
    async fn redis_worker_leases_only_available_preparation_capacity() {
        let Some(redis_url) = std::env::var("LLMSHIM_REDIS_URL").ok() else {
            return;
        };
        let provider = format!(
            "itest-preparation-capacity-{}",
            uuid::Uuid::new_v4().simple()
        );
        let client = redis::Client::open(redis_url.clone()).unwrap();
        let mut connection = ConnectionManager::new(client).await.unwrap();
        for key in [
            queue_key(&provider),
            processing_key(&provider),
            leased_key(&provider),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }

        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let first_dispatch_started = dispatch_started.notified();
        let release_dispatches = Arc::new(Semaphore::new(0));
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(LatchBlockedDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_dispatches: release_dispatches.clone(),
            }),
            unlimited(),
            GatewayConfig {
                max_concurrency_per_provider: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut submissions = Vec::new();
        for id in 0..3 {
            let gateway = gateway.clone();
            let provider = provider.clone();
            submissions.push(tokio::spawn(async move {
                gateway
                    .submit(GatewayRequest {
                        provider,
                        tier: 0,
                        permits: 1,
                        payload: serde_json::json!({"id": id}),
                    })
                    .await
            }));
        }
        for _ in 0..1_000 {
            let queued: u64 = connection.zcard(queue_key(&provider)).await.unwrap();
            if queued == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            connection
                .zcard::<_, u64>(queue_key(&provider))
                .await
                .unwrap(),
            3
        );

        let worker_handles = gateway.spawn_workers(vec![provider.clone()]);
        tokio::time::timeout(Duration::from_secs(2), first_dispatch_started)
            .await
            .expect("first leased dispatch should start");
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            connection
                .zcard::<_, u64>(processing_key(&provider))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .hlen::<_, u64>(leased_key(&provider))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(queue_key(&provider))
                .await
                .unwrap(),
            2
        );

        release_dispatches.add_permits(3);
        for submission in submissions {
            tokio::time::timeout(Duration::from_secs(2), submission)
                .await
                .expect("bounded dispatch should complete")
                .unwrap()
                .unwrap();
        }
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 3);
        for worker_handle in worker_handles {
            worker_handle.abort();
        }
        for key in [
            queue_key(&provider),
            processing_key(&provider),
            leased_key(&provider),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }
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
    async fn redis_client_idempotency_binds_owner_and_request() {
        let provider = "itest-client-idempotency";
        let Some(gateway) = test_gateway(provider).await else {
            return;
        };
        let original_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let changed_request_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "two"}),
        );
        let other_credential_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-b",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let reassigned_tenant_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-b",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let mut connection = gateway.conn.clone();
        let scoped_storage_key = scoped_idempotency_key(&original_context);
        let unsafe_legacy_key = "llmshim:gw:idem:client-key";
        let _: Result<i64, _> = connection.del(&scoped_storage_key).await;
        let _: Result<i64, _> = connection.del(unsafe_legacy_key).await;

        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
        gateway
            .scoped_idem_store(
                &original_context,
                &serde_json::json!({"private": "response"}),
                60,
            )
            .await;
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Replay(
                serde_json::json!({"private": "response"})
            )
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&changed_request_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Conflict
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&other_credential_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&reassigned_tenant_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let _: () = connection
            .set_ex(
                unsafe_legacy_key,
                serde_json::json!({"private": "legacy-response"}).to_string(),
                60,
            )
            .await
            .unwrap();
        let _: i64 = connection.del(&scoped_storage_key).await.unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let _: () = connection
            .set_ex(&scoped_storage_key, "not-json", 60)
            .await
            .unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let unknown_version = serde_json::json!({
            "version": 2,
            "request_fingerprint": "unused",
            "response": {"private": "unknown-version"}
        });
        let _: () = connection
            .set_ex(&scoped_storage_key, unknown_version.to_string(), 60)
            .await
            .unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    #[allow(deprecated)]
    async fn redis_generic_idempotency_api_uses_an_isolated_namespace() {
        let provider = "itest-generic-idempotency";
        let Some(gateway) = test_gateway(provider).await else {
            return;
        };
        let client_key = format!("generic-{}", uuid::Uuid::new_v4());
        let mut connection = gateway.conn.clone();
        let generic_key = generic_idempotency_key(&client_key);
        let unsafe_legacy_key = format!("llmshim:gw:idem:{client_key}");
        let _: Result<i64, _> = connection.del(&generic_key).await;
        let _: Result<i64, _> = connection.del(&unsafe_legacy_key).await;

        gateway
            .idem_put(&client_key, &serde_json::json!({"generic": true}), 60)
            .await;
        assert_eq!(
            gateway.idem_get(&client_key).await,
            Some(serde_json::json!({"generic": true}))
        );
        assert!(connection.exists::<_, bool>(&generic_key).await.unwrap());
        assert!(!connection
            .exists::<_, bool>(&unsafe_legacy_key)
            .await
            .unwrap());
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
