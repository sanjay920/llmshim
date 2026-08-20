//! Experimental priority-queue **gateway** (feature `gateway`).
//!
//! A fleet handling thousands of req/s can't fire every LLM call the instant it
//! arrives without blowing provider RPM/TPM limits. Instead of *rejecting* when
//! the token bucket is empty (what the proxy's `admission_control` does today),
//! the gateway *enqueues* each request into a per-provider priority queue and a
//! dispatcher sends the upstream call when capacity frees — ordered by priority
//! tier (paying customer > free) then FIFO within a tier.
//!
//! ## Shape
//!
//! ```text
//!  submit(req) ──enqueue──► [per-provider priority queue] ──dequeue──► dispatcher
//!      ▲ await oneshot                                                    │
//!      └──────────────────── result ◄──── Dispatch::dispatch ◄── RateLimiter.acquire
//! ```
//!
//! * **[`Scheduler`]** owns one lane (queue + dispatcher task) per provider, so a
//!   rate-limited OpenAI queue never blocks a ready Anthropic one.
//! * **[`RequestQueue`]** is the pluggable backend — [`InMemoryQueue`] is the
//!   zero-infra default; SQS / RabbitMQ / NATS impls slot in behind the same
//!   trait for a distributed fleet on AWS.
//! * The dispatcher is **event/timer-driven**: on an empty queue it awaits a
//!   notify; on a rate-limit miss it requeues and sleeps for exactly the
//!   [`RetryAfter`] the [`RateLimiter`] reports (waking early if new work
//!   arrives) — no busy-waiting, no `RateLimiter` changes.
//! * Reuses the proxy's [`RateLimiter`] (token buckets, per-provider RPM/TPM)
//!   and [`RetryAfter`]; the [`Dispatch`] trait is injected so the scheduler is
//!   testable without real HTTP.
//!
//! Known MVP simplifications (deferred): tier fairness/aging (a flood of
//! high-tier traffic can starve low tiers), streaming-through-queue, and
//! distributed queue backends. Priority tier is caller-supplied.

pub mod http;

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{oneshot, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::proxy::ratelimit::{RateKey, RateLimiter, RetryAfter};

/// Priority tier: higher dispatches first (e.g. `2` = paying, `0` = free).
pub type Tier = u8;

/// Ordering key for a queued job: **higher tier first, then FIFO by seqno**.
///
/// `BinaryHeap` is a max-heap, so `Ord` returns `Greater` for the job that
/// should dispatch first — higher `tier`, and for equal tiers the *lower*
/// `seqno` (enqueued earlier) wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PriorityKey {
    tier: Tier,
    seqno: u64,
}

impl Ord for PriorityKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tier
            .cmp(&other.tier)
            .then_with(|| other.seqno.cmp(&self.seqno))
    }
}

impl PartialOrd for PriorityKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A request submitted to the gateway.
pub struct GatewayRequest {
    /// Provider lane, e.g. `"openai"` / `"anthropic"`.
    pub provider: String,
    /// Priority tier (higher dispatches first).
    pub tier: Tier,
    /// Estimated token cost for TPM accounting (clamped to `>= 1`; use `1` for
    /// pure RPM limiting).
    pub permits: u32,
    /// Opaque payload handed verbatim to [`Dispatch::dispatch`].
    pub payload: Value,
}

/// Outcome of a [`Scheduler::submit`] that did not produce a response.
#[derive(Debug)]
pub enum GatewayError {
    /// The queue is full — shed load. Carries a suggested `Retry-After`.
    Overloaded(Duration),
    /// Waited past `max_wait` without being dispatched (the queued job is
    /// abandoned and will never burn a token).
    Timeout,
    /// The upstream dispatch failed.
    Upstream(String),
    /// The scheduler / dispatcher is gone.
    Shutdown,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Overloaded(d) => write!(f, "gateway overloaded, retry in {d:?}"),
            GatewayError::Timeout => write!(f, "gateway queue wait timed out"),
            GatewayError::Upstream(m) => write!(f, "upstream error: {m}"),
            GatewayError::Shutdown => write!(f, "gateway shutting down"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// Error returned by a [`Dispatch`] implementation.
pub struct DispatchError {
    pub message: String,
    /// Set when the upstream signalled a 429 so the scheduler can penalize the
    /// provider's token bucket before serving the next job.
    pub retry_after: Option<Duration>,
}

impl DispatchError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry_after: None,
        }
    }
}

/// The upstream work executed once a job is admitted. Injected into the
/// [`Scheduler`] so it can be exercised without real network calls.
#[async_trait]
pub trait Dispatch: Send + Sync {
    async fn dispatch(&self, provider: &str, payload: Value) -> Result<Value, DispatchError>;
}

/// A unit of queued work. The `tx` half of a oneshot delivers the result back to
/// the awaiting [`Scheduler::submit`] caller; if the caller goes away (timeout /
/// client disconnect) `tx.is_closed()` flips and the dispatcher skips the job
/// without spending a token.
pub struct Job {
    key: PriorityKey,
    permits: u32,
    payload: Value,
    tx: oneshot::Sender<Result<Value, GatewayError>>,
    /// Fires the instant the dispatcher commits to the upstream call, so
    /// [`Scheduler::submit`]'s `max_wait` bounds only **queue residence** — not
    /// the (possibly long) upstream call itself.
    started: oneshot::Sender<()>,
}

impl Job {
    fn is_cancelled(&self) -> bool {
        self.tx.is_closed()
    }
}

// Heap ordering is entirely by priority key.
impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for Job {}
impl Ord for Job {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}
impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Pluggable queue backend. One instance per provider lane.
///
/// The in-memory default is [`InMemoryQueue`]; a distributed backend (SQS /
/// NATS) implements the same contract — `enqueue` is the producer side,
/// `dequeue` / `requeue` / `notified` drive a single dispatcher.
#[async_trait]
pub trait RequestQueue: Send + Sync {
    /// Enqueue new work. Returns `Err(job)` (handing the job back) when the
    /// queue is at capacity so the caller can shed load.
    fn enqueue(&self, job: Job) -> Result<(), Job>;

    /// Await and remove the highest-priority **live** job (cancelled jobs are
    /// dropped). Resolves as soon as one is available.
    async fn dequeue(&self) -> Job;

    /// Return a dequeued job (e.g. it was rate-limited) without a capacity
    /// check — it keeps its original priority/seqno, so it lands back in place.
    /// Does **not** wake the dispatcher (the dispatcher is the only consumer and
    /// is about to wait out its backoff).
    fn requeue(&self, job: Job);

    /// Resolves when the queue's contents may have changed (a new `enqueue`),
    /// so a dispatcher waiting out a rate-limit backoff can wake early.
    async fn notified(&self);

    /// Current queued depth (metrics / tests).
    fn depth(&self) -> usize;
}

/// Zero-infra in-memory backend: a priority `BinaryHeap` + a `Notify`.
pub struct InMemoryQueue {
    heap: Mutex<BinaryHeap<Job>>,
    notify: Notify,
    max_depth: usize,
}

impl InMemoryQueue {
    pub fn new(max_depth: usize) -> Self {
        Self {
            heap: Mutex::new(BinaryHeap::new()),
            notify: Notify::new(),
            max_depth: max_depth.max(1),
        }
    }
}

#[async_trait]
impl RequestQueue for InMemoryQueue {
    fn enqueue(&self, job: Job) -> Result<(), Job> {
        {
            let mut heap = self.heap.lock().unwrap();
            if heap.len() >= self.max_depth {
                return Err(job);
            }
            heap.push(job);
        }
        // Wake a dispatcher parked on an empty queue or a backoff sleep.
        self.notify.notify_one();
        Ok(())
    }

    async fn dequeue(&self) -> Job {
        loop {
            {
                let mut heap = self.heap.lock().unwrap();
                // Drop cancelled jobs sitting at the top.
                while heap.peek().map(|j| j.is_cancelled()).unwrap_or(false) {
                    heap.pop();
                }
                if let Some(job) = heap.pop() {
                    return job;
                }
            }
            self.notify.notified().await;
        }
    }

    fn requeue(&self, job: Job) {
        // No depth check and no notify: this is the dispatcher putting back a
        // job it just took; notifying here would spin the backoff loop.
        self.heap.lock().unwrap().push(job);
    }

    async fn notified(&self) {
        self.notify.notified().await;
    }

    fn depth(&self) -> usize {
        self.heap.lock().unwrap().len()
    }
}

/// Scheduler tuning. Sensible zero-config defaults via [`Default`].
#[derive(Clone)]
pub struct GatewayConfig {
    /// Max queued jobs per provider before `submit` sheds with `Overloaded`.
    pub max_queue_depth: usize,
    /// Max time a job may wait to be dispatched before `submit` returns
    /// `Timeout` (and abandons the queued job).
    pub max_wait: Duration,
    /// `Retry-After` suggested on an `Overloaded` shed.
    pub overloaded_retry_after: Duration,
    /// Max concurrent in-flight upstream calls per provider.
    pub max_concurrency_per_provider: usize,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            max_queue_depth: 10_000,
            max_wait: Duration::from_secs(30),
            overloaded_retry_after: Duration::from_secs(1),
            max_concurrency_per_provider: 256,
        }
    }
}

impl GatewayConfig {
    /// Read tuning from the environment, falling back to [`Default`] per field:
    /// `LLMSHIM_GATEWAY_QUEUE_DEPTH`, `LLMSHIM_GATEWAY_MAX_WAIT_MS`,
    /// `LLMSHIM_GATEWAY_MAX_CONCURRENCY`.
    pub fn from_env() -> Self {
        let d = Self::default();
        let usize_env = |k: &str, fallback: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(fallback)
        };
        Self {
            max_queue_depth: usize_env("LLMSHIM_GATEWAY_QUEUE_DEPTH", d.max_queue_depth),
            max_wait: std::env::var("LLMSHIM_GATEWAY_MAX_WAIT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis)
                .unwrap_or(d.max_wait),
            overloaded_retry_after: d.overloaded_retry_after,
            max_concurrency_per_provider: usize_env(
                "LLMSHIM_GATEWAY_MAX_CONCURRENCY",
                d.max_concurrency_per_provider,
            ),
        }
    }
}

struct Lane {
    queue: Arc<dyn RequestQueue>,
}

/// Priority-queue scheduler in front of the LLM calls. Cheaply cloneable via
/// `Arc`; one dispatcher task is spawned per provider lane on first use.
pub struct Scheduler {
    limiter: Arc<dyn RateLimiter>,
    dispatch: Arc<dyn Dispatch>,
    config: GatewayConfig,
    lanes: Mutex<HashMap<String, Lane>>,
    seq: AtomicU64,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Scheduler {
    /// Build a scheduler over an injected rate limiter and dispatcher.
    pub fn new(
        config: GatewayConfig,
        limiter: Arc<dyn RateLimiter>,
        dispatch: Arc<dyn Dispatch>,
    ) -> Arc<Self> {
        Arc::new(Self {
            limiter,
            dispatch,
            config,
            lanes: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            handles: Mutex::new(Vec::new()),
        })
    }

    /// Enqueue a request and await its result. The queueing is internal: the
    /// caller sees a normal response, an `Overloaded` shed, or a `Timeout`.
    pub async fn submit(self: &Arc<Self>, req: GatewayRequest) -> Result<Value, GatewayError> {
        let queue = self.lane_for(&req.provider);
        let seqno = self.seq.fetch_add(1, AtomicOrdering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let (started_tx, started_rx) = oneshot::channel();
        let job = Job {
            key: PriorityKey {
                tier: req.tier,
                seqno,
            },
            permits: req.permits.max(1),
            payload: req.payload,
            tx,
            started: started_tx,
        };

        if queue.enqueue(job).is_err() {
            return Err(GatewayError::Overloaded(self.config.overloaded_retry_after));
        }

        // `max_wait` bounds only how long the job may sit *in the queue*. Once
        // the dispatcher commits (fires `started`), we wait for the full result
        // with no deadline — a slow-but-valid upstream call must not time out.
        // On a queue-wait timeout `rx` drops → `tx.is_closed()` flips → the
        // dispatcher skips the job without spending a token.
        tokio::select! {
            biased;
            started = started_rx => match started {
                // Committed (or the dispatcher dropped it): await the outcome.
                Ok(()) | Err(_) => match rx.await {
                    Ok(result) => result,
                    Err(_) => Err(GatewayError::Shutdown),
                },
            },
            _ = tokio::time::sleep(self.config.max_wait) => Err(GatewayError::Timeout),
        }
    }

    /// Current queued depth for a provider (0 if the lane doesn't exist yet).
    pub fn queue_depth(&self, provider: &str) -> usize {
        self.lanes
            .lock()
            .unwrap()
            .get(provider)
            .map(|l| l.queue.depth())
            .unwrap_or(0)
    }

    /// Get or create the lane (queue + dispatcher task) for a provider.
    fn lane_for(self: &Arc<Self>, provider: &str) -> Arc<dyn RequestQueue> {
        let mut lanes = self.lanes.lock().unwrap();
        if let Some(lane) = lanes.get(provider) {
            return lane.queue.clone();
        }
        let queue: Arc<dyn RequestQueue> =
            Arc::new(InMemoryQueue::new(self.config.max_queue_depth));
        lanes.insert(
            provider.to_string(),
            Lane {
                queue: queue.clone(),
            },
        );
        let handle = tokio::spawn(dispatcher_loop(
            provider.to_string(),
            queue.clone(),
            self.limiter.clone(),
            self.dispatch.clone(),
            self.config.max_concurrency_per_provider,
        ));
        self.handles.lock().unwrap().push(handle);
        queue
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        for handle in self.handles.lock().unwrap().drain(..) {
            handle.abort();
        }
    }
}

/// Per-provider dispatcher. Pops the highest-priority job, waits out the
/// provider's rate limit if needed, then fires the upstream call (bounded by a
/// per-provider concurrency semaphore).
async fn dispatcher_loop(
    provider: String,
    queue: Arc<dyn RequestQueue>,
    limiter: Arc<dyn RateLimiter>,
    dispatch: Arc<dyn Dispatch>,
    max_concurrency: usize,
) {
    let key = RateKey::provider(provider.clone());
    let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));

    loop {
        let job = queue.dequeue().await;
        if job.is_cancelled() {
            continue; // caller gave up while queued — no token spent
        }

        // Bound in-flight upstream calls *before* spending a rate token, so a
        // saturated concurrency semaphore never wastes a token nor holds one
        // idle while a lane waits for a slot.
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed (shutdown)
        };
        if job.is_cancelled() {
            continue; // gave up while waiting for a concurrency slot
        }

        match limiter.acquire(&key, job.permits).await {
            Ok(()) => {
                if job.is_cancelled() {
                    continue; // gave up during the token wait
                }
                let Job {
                    payload,
                    tx,
                    started,
                    ..
                } = job;
                // Queue residence is over — release the caller's `max_wait`.
                let _ = started.send(());
                let dispatch = dispatch.clone();
                let limiter = limiter.clone();
                let provider = provider.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    match dispatch.dispatch(&provider, payload).await {
                        Ok(value) => {
                            let _ = tx.send(Ok(value));
                        }
                        Err(err) => {
                            if let Some(retry_after) = err.retry_after {
                                limiter
                                    .penalize(&RateKey::provider(provider.clone()), retry_after)
                                    .await;
                            }
                            let _ = tx.send(Err(GatewayError::Upstream(err.message)));
                        }
                    }
                });
            }
            Err(RetryAfter(wait)) => {
                // Release the concurrency slot, requeue (keeps its priority),
                // and sleep for exactly the provider's reported backoff — waking
                // early if new (possibly higher-priority) work arrives.
                drop(permit);
                queue.requeue(job);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = queue.notified() => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap as Map;

    // ---- test doubles -------------------------------------------------------

    /// A rate limiter with a controllable per-provider permit balance. `acquire`
    /// consumes permits when available, else reports a 1s backoff. No clock, so
    /// tests stay deterministic under `tokio::time` pause.
    struct FakeLimiter {
        permits: Mutex<Map<String, i64>>,
        default: i64,
    }
    impl FakeLimiter {
        fn new(default: i64) -> Self {
            Self {
                permits: Mutex::new(Map::new()),
                default,
            }
        }
        fn set(&self, provider: &str, n: i64) {
            self.permits.lock().unwrap().insert(provider.to_string(), n);
        }
    }
    #[async_trait]
    impl RateLimiter for FakeLimiter {
        async fn acquire(&self, key: &RateKey, permits: u32) -> Result<(), RetryAfter> {
            let mut map = self.permits.lock().unwrap();
            let bal = map.entry(key.provider.clone()).or_insert(self.default);
            if *bal >= permits as i64 {
                *bal -= permits as i64;
                Ok(())
            } else {
                Err(RetryAfter(Duration::from_secs(1)))
            }
        }
        async fn penalize(&self, _key: &RateKey, _retry_after: Duration) {}
    }

    /// Records the order in which payloads are dispatched (by their `id`).
    struct RecordingDispatch {
        order: Arc<Mutex<Vec<u64>>>,
    }
    #[async_trait]
    impl Dispatch for RecordingDispatch {
        async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
            let id = payload["id"].as_u64().unwrap();
            self.order.lock().unwrap().push(id);
            Ok(json!({ "id": id }))
        }
    }

    fn scheduler(
        limiter: Arc<FakeLimiter>,
        order: Arc<Mutex<Vec<u64>>>,
        config: GatewayConfig,
    ) -> Arc<Scheduler> {
        Scheduler::new(config, limiter, Arc::new(RecordingDispatch { order }))
    }

    async fn yield_many() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// Wait until a provider's queue reaches `target` depth, bounded so a
    /// mis-set precondition panics instead of hanging the test suite.
    async fn wait_for_depth(sched: &Arc<Scheduler>, provider: &str, target: usize) {
        for _ in 0..10_000 {
            if sched.queue_depth(provider) >= target {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("queue for {provider} never reached depth {target} (dispatcher gated?)");
    }

    /// Spawn `submit` calls in a controlled order so seqno == submission order
    /// (each lands before the next is offered). Requires the limiter be gated
    /// (0 permits) so nothing dispatches yet.
    async fn enqueue_ordered(
        sched: &Arc<Scheduler>,
        provider: &str,
        items: &[(u64, Tier)],
    ) -> Vec<JoinHandle<Result<Value, GatewayError>>> {
        let mut handles = Vec::new();
        for (i, &(id, tier)) in items.iter().enumerate() {
            let s = sched.clone();
            let p = provider.to_string();
            handles.push(tokio::spawn(async move {
                s.submit(GatewayRequest {
                    provider: p,
                    tier,
                    permits: 1,
                    payload: json!({ "id": id }),
                })
                .await
            }));
            // Wait for this job to land before offering the next. Bounded so a
            // bad precondition (e.g. a non-gated limiter that dispatches the job
            // before it can be observed) fails fast instead of hanging.
            wait_for_depth(sched, provider, i + 1).await;
        }
        handles
    }

    // ---- queue-level (fully deterministic, no dispatcher) --------------------

    fn dummy_job(tier: Tier, seqno: u64) -> (Job, oneshot::Receiver<Result<Value, GatewayError>>) {
        let (tx, rx) = oneshot::channel();
        let (started, _started_rx) = oneshot::channel();
        (
            Job {
                key: PriorityKey { tier, seqno },
                permits: 1,
                payload: json!({ "id": seqno }),
                tx,
                started,
            },
            rx,
        )
    }

    #[tokio::test]
    async fn queue_dequeues_highest_tier_then_fifo() {
        let q = InMemoryQueue::new(100);
        // Keep receivers alive so jobs aren't treated as cancelled.
        let mut keep = Vec::new();
        for (tier, seq) in [(1u8, 0u64), (3, 1), (1, 2), (2, 3), (3, 4)] {
            let (job, rx) = dummy_job(tier, seq);
            keep.push(rx);
            assert!(q.enqueue(job).is_ok());
        }
        // tier3 first (seq1 before seq4), then tier2, then tier1 (seq0 before seq2).
        let mut got = Vec::new();
        for _ in 0..5 {
            got.push(q.dequeue().await.key);
        }
        let order: Vec<(u8, u64)> = got.iter().map(|k| (k.tier, k.seqno)).collect();
        assert_eq!(order, vec![(3, 1), (3, 4), (2, 3), (1, 0), (1, 2)]);
    }

    #[tokio::test]
    async fn queue_sheds_when_full() {
        let q = InMemoryQueue::new(2);
        let (j1, _r1) = dummy_job(0, 0);
        let (j2, _r2) = dummy_job(0, 1);
        let (j3, _r3) = dummy_job(0, 2);
        assert!(q.enqueue(j1).is_ok());
        assert!(q.enqueue(j2).is_ok());
        assert!(q.enqueue(j3).is_err(), "third enqueue should shed");
        assert_eq!(q.depth(), 2);
    }

    // ---- scheduler integration ---------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn max_wait_bounds_queue_time_not_upstream_call() {
        // Capacity is available immediately, but the upstream call is far slower
        // than `max_wait`. The job leaves the queue at once, so it must NOT time
        // out — `max_wait` bounds queue residence only.
        struct SlowDispatch;
        #[async_trait]
        impl Dispatch for SlowDispatch {
            async fn dispatch(
                &self,
                _provider: &str,
                payload: Value,
            ) -> Result<Value, DispatchError> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(payload)
            }
        }
        let limiter = Arc::new(FakeLimiter::new(1000)); // plenty of capacity
        let config = GatewayConfig {
            max_wait: Duration::from_secs(5),
            ..Default::default()
        };
        let sched = Scheduler::new(config, limiter, Arc::new(SlowDispatch));

        let s = sched.clone();
        let handle = tokio::spawn(async move {
            s.submit(GatewayRequest {
                provider: "p".into(),
                tier: 0,
                permits: 1,
                payload: json!({ "id": 7 }),
            })
            .await
        });

        // Advance well past both max_wait (5s) and the upstream call (60s).
        yield_many().await;
        tokio::time::advance(Duration::from_secs(61)).await;
        yield_many().await;

        let result = handle.await.unwrap();
        assert!(
            matches!(&result, Ok(v) if v["id"] == 7),
            "slow upstream call must complete, not time out: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dispatches_in_priority_then_fifo_order() {
        let limiter = Arc::new(FakeLimiter::new(0)); // gated: nothing dispatches yet
        let order = Arc::new(Mutex::new(Vec::new()));
        let sched = scheduler(limiter.clone(), order.clone(), GatewayConfig::default());

        // Enqueue in a known order → seqno order: id1..id4.
        let handles = enqueue_ordered(&sched, "p", &[(1, 1), (2, 3), (3, 1), (4, 2)]).await;

        // Release capacity and let the backoff sleeps expire.
        limiter.set("p", 100);
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_many().await;
        for h in handles {
            h.await.unwrap().unwrap();
        }

        // tier3 (id2), tier2 (id4), then tier1 FIFO (id1 before id3).
        assert_eq!(*order.lock().unwrap(), vec![2, 4, 1, 3]);
    }

    #[tokio::test(start_paused = true)]
    async fn gates_on_rate_limit_then_dispatches_on_refill() {
        let limiter = Arc::new(FakeLimiter::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let sched = scheduler(limiter.clone(), order.clone(), GatewayConfig::default());

        // Enqueue both while gated (default 0 permits), then release capacity.
        let handles = enqueue_ordered(&sched, "p", &[(1, 0), (2, 0)]).await;

        // One token → only the first job dispatches; the second re-gates.
        limiter.set("p", 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_many().await;
        assert_eq!(*order.lock().unwrap(), vec![1]);

        // Refill and advance past the backoff → the second dispatches.
        limiter.set("p", 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_many().await;
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        for h in handles {
            h.await.unwrap().unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn per_provider_independence() {
        let limiter = Arc::new(FakeLimiter::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let sched = scheduler(limiter.clone(), order.clone(), GatewayConfig::default());

        limiter.set("slow", 0); // slow provider is fully gated
        limiter.set("fast", 100); // fast provider has capacity

        let slow = {
            let s = sched.clone();
            tokio::spawn(async move {
                s.submit(GatewayRequest {
                    provider: "slow".into(),
                    tier: 0,
                    permits: 1,
                    payload: json!({ "id": 99 }),
                })
                .await
            })
        };
        let fast = {
            let s = sched.clone();
            tokio::spawn(async move {
                s.submit(GatewayRequest {
                    provider: "fast".into(),
                    tier: 0,
                    permits: 1,
                    payload: json!({ "id": 1 }),
                })
                .await
            })
        };

        yield_many().await;
        // fast dispatched despite slow being gated.
        assert_eq!(*order.lock().unwrap(), vec![1]);
        fast.await.unwrap().unwrap();
        slow.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_when_never_dispatched() {
        let limiter = Arc::new(FakeLimiter::new(0)); // permanently gated
        let order = Arc::new(Mutex::new(Vec::new()));
        let config = GatewayConfig {
            max_wait: Duration::from_secs(2),
            ..Default::default()
        };
        let sched = scheduler(limiter.clone(), order.clone(), config);

        let s = sched.clone();
        let handle = tokio::spawn(async move {
            s.submit(GatewayRequest {
                provider: "p".into(),
                tier: 0,
                permits: 1,
                payload: json!({ "id": 1 }),
            })
            .await
        });

        yield_many().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        yield_many().await;

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(GatewayError::Timeout)));
        assert!(order.lock().unwrap().is_empty(), "nothing should dispatch");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_job_is_not_dispatched() {
        let limiter = Arc::new(FakeLimiter::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let sched = scheduler(limiter.clone(), order.clone(), GatewayConfig::default());

        // Enqueue while gated, then abort the caller (drops rx → job cancelled).
        let s = sched.clone();
        let handle = tokio::spawn(async move {
            s.submit(GatewayRequest {
                provider: "p".into(),
                tier: 0,
                permits: 1,
                payload: json!({ "id": 1 }),
            })
            .await
        });
        wait_for_depth(&sched, "p", 1).await;
        handle.abort();
        yield_many().await;

        // Open capacity: the dispatcher should skip the cancelled job.
        limiter.set("p", 100);
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_many().await;
        assert!(
            order.lock().unwrap().is_empty(),
            "cancelled job must not dispatch"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sheds_when_queue_full() {
        let limiter = Arc::new(FakeLimiter::new(0)); // gated so the queue fills
        let order = Arc::new(Mutex::new(Vec::new()));
        let config = GatewayConfig {
            max_queue_depth: 2,
            ..Default::default()
        };
        let sched = scheduler(limiter.clone(), order.clone(), config);

        // Fill the queue to capacity.
        let _held = enqueue_ordered(&sched, "p", &[(1, 0), (2, 0)]).await;

        // The next submit sheds immediately.
        let result = sched
            .submit(GatewayRequest {
                provider: "p".into(),
                tier: 0,
                permits: 1,
                payload: json!({ "id": 3 }),
            })
            .await;
        assert!(matches!(result, Err(GatewayError::Overloaded(_))));
    }
}
