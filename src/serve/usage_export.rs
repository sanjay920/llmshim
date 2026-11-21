//! The usage push (design note 0051): each provider attempt's usage record, in batches, to the control
//! plane's request ledger (`POST /v1/usage/batch`, with the ingest token).
//!
//! It exists to be unable to hurt serving. `record` only appends to a bounded in-memory queue (dropping
//! the **oldest** record, counted, when it is full); a thread of its own, with a runtime of its own, sends
//! the batches, so a slow or dead control plane costs a caller nothing and the server's runtime is never
//! asked for anything. Delivery is at least once and the ledger stores each attempt once, so a batch that
//! may or may not have arrived is simply sent again.

use crate::gateway::metrics;
use crate::serve::blobstore::s3::Source;
use crate::serve::config::SecretRef;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const RECORDS: &str = "llmshim_usage_export_records_total";
pub const FAILURES: &str = "llmshim_usage_export_failures_total";
pub const QUEUE_DEPTH: &str = "llmshim_usage_export_queue_depth";

const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The most one batch's records may add up to, as JSON. The control plane refuses a body over 2 MiB; a record of a
/// capturing client can hold thousands of segments, so a batch is cut by size as well as by count.
const BATCH_BYTES: usize = 1 << 20;
/// How long `close` waits for the last batches.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// Re-exported so a control-plane test can drive the exporter end to end.
pub use crate::gateway::structure::Segment;
pub use crate::gateway::usage::{UsageRecord, UsageSink};

/// What `[usage_export]` says.
pub struct ExportSettings<'a> {
    /// The control plane's origin, `https://host[:port]`.
    pub url: &'a str,
    pub token: &'a SecretRef,
    pub batch_size: usize,
    pub flush_interval: Duration,
    /// The most records held in memory; more drops the oldest.
    pub queue_size: usize,
    pub ca_pem: Option<&'a [u8]>,
}

struct Shared {
    queue: Mutex<VecDeque<Value>>,
    wake: Condvar,
    closed: AtomicBool,
    capacity: usize,
    batch: usize,
}

pub struct UsageExporter {
    shared: Arc<Shared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Why a send did not complete.
enum Failure {
    /// Worth trying again later: the control plane is down, busy, or refuses this credential or address
    /// (which an operator can fix, so nothing is thrown away meanwhile).
    Retry(&'static str),
    /// The control plane says the batch itself is wrong: sending it again cannot help.
    Poison(&'static str),
    /// The control plane refused the body as too large: a batch of several records is split and sent again,
    /// a single record that is too large is dropped.
    TooLarge,
}

struct Sender {
    client: reqwest::Client,
    endpoint: String,
    token: Source,
}

impl Sender {
    async fn send(&self, records: &[Value]) -> Result<Value, Failure> {
        let token = self.token.read().map_err(|_| Failure::Retry("token"))?;
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(token)
            .json(&json!({"records": records}))
            .send()
            .await
            .map_err(|_| Failure::Retry("transport"))?;
        let status = response.status().as_u16();
        match status {
            200 => Ok(response.json::<Value>().await.unwrap_or(Value::Null)),
            413 => Err(Failure::TooLarge),
            400 | 422 => Err(Failure::Poison("bad_request")),
            401 | 403 => Err(Failure::Retry("unauthorized")),
            404 => Err(Failure::Retry("not_found")),
            429 => Err(Failure::Retry("rate_limited")),
            _ => Err(Failure::Retry(if status >= 500 {
                "server_error"
            } else {
                "status"
            })),
        }
    }
}

impl UsageExporter {
    pub fn new(
        settings: &ExportSettings<'_>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let token = Source::new(settings.token, env).map_err(|m| format!("token: {m}"))?;
        // Read once now: a wrong path or an unset variable fails at start, not at the first flush.
        token
            .read()
            .map_err(|_| "token cannot be read".to_string())?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20));
        if let Some(pem) = settings.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| "ca_file holds no readable certificate".to_string())?;
            if certificates.is_empty() {
                return Err("ca_file holds no certificate".into());
            }
            builder = builder.tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let sender = Sender {
            client: builder
                .build()
                .map_err(|_| "cannot build the HTTP client".to_string())?,
            endpoint: format!("{}/v1/usage/batch", settings.url.trim_end_matches('/')),
            token,
        };
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            capacity: settings.queue_size,
            batch: settings.batch_size,
        });
        metrics::gauge_set(QUEUE_DEPTH, &[], 0);
        let flush_every = settings.flush_interval;
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("usage-export".into())
            .spawn(move || run(&worker_shared, &sender, flush_every))
            .map_err(|_| "cannot start the export thread".to_string())?;
        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Set the flag and wake the worker. The store and the notify are made under the queue lock: the
    /// worker checks the flag and starts waiting while holding it, so without the lock the notify could
    /// land between the check and the wait and be lost.
    fn signal_close(&self) {
        let _queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }

    /// Send what is queued (bounded wait), then stop. Call once, after the listeners have stopped.
    pub fn close(&self) {
        self.signal_close();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let deadline = Instant::now() + CLOSE_WAIT;
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }

    /// Records waiting to be sent (for tests and the depth gauge).
    pub fn queued(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl UsageSink for UsageExporter {
    fn record(&self, record: &UsageRecord) {
        let Ok(value) = serde_json::to_value(record) else {
            return;
        };
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(value);
        if queue.len() > self.shared.capacity {
            queue.pop_front();
            metrics::add(RECORDS, &[("outcome", "dropped_queue_full")], 1);
        }
        metrics::gauge_set(QUEUE_DEPTH, &[], queue.len() as i64);
        if queue.len() >= self.shared.batch {
            self.shared.wake.notify_one();
        }
    }
}

fn jitter(base: Duration) -> Duration {
    use ring::rand::SecureRandom;
    let mut byte = [0u8; 1];
    let _ = ring::rand::SystemRandom::new().fill(&mut byte);
    // ±20%.
    let factor = 0.8 + f64::from(byte[0]) / 255.0 * 0.4;
    base.mul_f64(factor)
}

fn run(shared: &Shared, sender: &Sender, flush_every: Duration) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    let mut backoff = Duration::ZERO;
    // How many records a batch may hold now: the configured size, less after the control plane called a body too
    // large, growing back as batches are accepted.
    let mut limit = shared.batch;
    let mut not_before = Instant::now();
    let mut last_flush = Instant::now();
    loop {
        // Wait until there is a reason to send.
        let batch = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let closed = shared.closed.load(Ordering::SeqCst);
                let due = queue.len() >= shared.batch || last_flush.elapsed() >= flush_every;
                if (closed || (due && Instant::now() >= not_before)) && !queue.is_empty() {
                    break;
                }
                if closed && queue.is_empty() {
                    return;
                }
                let wait = if queue.is_empty() {
                    flush_every
                } else {
                    not_before
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(20))
                        .min(flush_every)
                };
                queue = shared
                    .wake
                    .wait_timeout(queue, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            // The size is measured here, on the worker, never on a request's path.
            let mut batch: Vec<Value> = Vec::new();
            let mut bytes = 0usize;
            while let Some(front) = queue.front() {
                if batch.len() >= limit {
                    break;
                }
                let size = front.to_string().len();
                if !batch.is_empty() && bytes + size > BATCH_BYTES {
                    break;
                }
                bytes += size;
                batch.extend(queue.pop_front());
            }
            metrics::gauge_set(QUEUE_DEPTH, &[], queue.len() as i64);
            batch
        };
        last_flush = Instant::now();
        match runtime.block_on(sender.send(&batch)) {
            Ok(answer) => {
                backoff = Duration::ZERO;
                limit = (limit * 2).min(shared.batch);
                let n = |field: &str| answer[field].as_u64().unwrap_or(0);
                for (outcome, count) in [
                    ("sent", n("accepted")),
                    ("duplicate", n("duplicate")),
                    ("rejected_by_server", n("rejected")),
                ] {
                    if count > 0 {
                        metrics::add(RECORDS, &[("outcome", outcome)], count);
                    }
                }
            }
            Err(Failure::TooLarge) if batch.len() > 1 && !shared.closed.load(Ordering::SeqCst) => {
                // Send the same records again in smaller batches, in order.
                metrics::incr(FAILURES, &[("class", "too_large")]);
                limit = (batch.len() / 2).max(1);
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                for record in batch.into_iter().rev() {
                    queue.push_front(record);
                }
            }
            Err(Failure::TooLarge) => {
                // One record the control plane will not take, or a shutdown: dropped and counted.
                metrics::incr(FAILURES, &[("class", "too_large")]);
                metrics::add(
                    RECORDS,
                    &[("outcome", "dropped_refused")],
                    batch.len() as u64,
                );
            }
            Err(Failure::Poison(class)) => {
                metrics::incr(FAILURES, &[("class", class)]);
                metrics::add(
                    RECORDS,
                    &[("outcome", "dropped_refused")],
                    batch.len() as u64,
                );
            }
            Err(Failure::Retry(class)) => {
                metrics::incr(FAILURES, &[("class", class)]);
                if shared.closed.load(Ordering::SeqCst) {
                    // Shutting down: one attempt was made; what is left is lost, and counted.
                    let left =
                        batch.len() + shared.queue.lock().unwrap_or_else(|e| e.into_inner()).len();
                    metrics::add(RECORDS, &[("outcome", "dropped_at_shutdown")], left as u64);
                    return;
                }
                // Back to the front, in order, so nothing is reordered; the queue's bound still applies
                // (and drops the oldest), so a long outage cannot grow memory.
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                for record in batch.into_iter().rev() {
                    queue.push_front(record);
                }
                while queue.len() > shared.capacity {
                    queue.pop_front();
                    metrics::add(RECORDS, &[("outcome", "dropped_queue_full")], 1);
                }
                backoff = if backoff.is_zero() {
                    BACKOFF_FIRST
                } else {
                    (backoff * 2).min(BACKOFF_MAX)
                };
                not_before = Instant::now() + jitter(backoff);
                if class == "unauthorized" || class == "not_found" {
                    eprintln!(
                        "usage export: the control plane answered {class}: check [usage_export] url and the ingest token, and that it runs with --ingest-token-file"
                    );
                }
            }
        }
    }
}

impl Drop for UsageExporter {
    /// A dropped exporter never leaves its worker running: it is asked to stop (it sends what it can
    /// first) without waiting for it. `close` is still how a server stops it in order.
    fn drop(&mut self) {
        self.signal_close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;
    use std::sync::atomic::AtomicUsize;

    fn usage(id: &str) -> UsageRecord {
        UsageRecord {
            event: "usage",
            ts: "2026-11-19T10:00:00Z".into(),
            attempt_id: id.into(),
            tenant: Some("payments".into()),
            key: Some("checkout".into()),
            tags: None,
            request_id: Some(format!("req-{id}")),
            deployment: None,
            provider: "openai".into(),
            model: "gpt-6-sol".into(),
            kind: "completion",
            outcome: "completed",
            http_status: None,
            duration_ms: 1200,
            input_tokens: 1000,
            output_tokens: 50,
            reasoning_tokens: None,
            uncached_input_tokens: Some(200),
            cache_read_tokens: 800,
            cache_write_tokens: 0,
            cost_usd: Some(0.01),
            cost_source: Some("provider".into()),
            usage_reported: true,
            admission: "fleet",
            structure: None,
            payload: None,
        }
    }

    fn token(value: &str) -> SecretRef {
        let path = std::env::temp_dir().join(format!("llmshim-ue-token-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("{value}\n")).unwrap();
        SecretRef {
            env: None,
            file: Some(path),
        }
    }

    fn exporter(
        url: &str,
        token: &SecretRef,
        batch: usize,
        flush_ms: u64,
        cap: usize,
    ) -> UsageExporter {
        UsageExporter::new(
            &ExportSettings {
                url,
                token,
                batch_size: batch,
                flush_interval: Duration::from_millis(flush_ms),
                queue_size: cap,
                ca_pem: None,
            },
            &|_| None,
        )
        .unwrap()
    }

    /// The attempt ids of every batch a mock control plane received, in order.
    type Batches = Arc<Mutex<Vec<Vec<String>>>>;

    fn ledger(server: &mut mockito::ServerGuard) -> (Batches, Arc<AtomicUsize>) {
        let seen: Batches = Default::default();
        let hits = Arc::new(AtomicUsize::new(0));
        let (record, count) = (seen.clone(), hits.clone());
        server
            .mock("POST", "/v1/usage/batch")
            .match_header("authorization", Matcher::Regex("^Bearer ".into()))
            .with_body_from_request(move |request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let ids: Vec<String> = body["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["attempt_id"].as_str().unwrap().to_owned())
                    .collect();
                count.fetch_add(1, Ordering::SeqCst);
                record.lock().unwrap().push(ids.clone());
                json!({"accepted": ids.len(), "duplicate": 0, "rejected": 0})
                    .to_string()
                    .into_bytes()
            })
            .expect_at_least(0)
            .create();
        (seen, hits)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_batch_is_cut_by_size_and_a_body_called_too_large_is_split_not_dropped() {
        let mut server = mockito::Server::new_async().await;
        let seen: Batches = Default::default();
        let record = seen.clone();
        // The control plane takes one record at a time and calls anything bigger too large.
        // (mockito answers with the most recently made matching mock, so the narrow one is made last.)
        server
            .mock("POST", "/v1/usage/batch")
            .match_header("authorization", Matcher::Regex("^Bearer ".into()))
            .with_body_from_request(move |request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let ids: Vec<String> = body["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["attempt_id"].as_str().unwrap().to_owned())
                    .collect();
                record.lock().unwrap().push(ids.clone());
                json!({"accepted": ids.len(), "duplicate": 0, "rejected": 0})
                    .to_string()
                    .into_bytes()
            })
            .expect_at_least(0)
            .create();
        server
            .mock("POST", "/v1/usage/batch")
            .match_header("authorization", Matcher::Regex("^Bearer ".into()))
            .match_request(|request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                body["records"].as_array().unwrap().len() > 1
            })
            .with_status(413)
            .expect_at_least(0)
            .create();
        let token = token("ingest-token-0123456789abc");
        let exporter = exporter(&server.url(), &token, 8, 200, 1000);
        for id in ["z1", "z2", "z3", "z4", "z5"] {
            exporter.record(&usage(id));
        }
        until("every record to arrive one at a time", || {
            seen.lock().unwrap().concat().len() == 5
        })
        .await;
        assert_eq!(
            seen.lock().unwrap().concat(),
            ["z1", "z2", "z3", "z4", "z5"],
            "in order, none lost"
        );
        assert!(
            seen.lock().unwrap().iter().all(|b| b.len() == 1),
            "{:?}",
            seen.lock().unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_that_add_up_to_more_than_a_batch_may_hold_go_in_separate_batches() {
        let mut server = mockito::Server::new_async().await;
        let (seen, _) = ledger(&mut server);
        let token = token("ingest-token-0123456789abc");
        let exporter = exporter(&server.url(), &token, 50, 200, 1000);
        // Four records of about 470 KB each: more than one batch's size, far fewer than its count.
        let segments = vec![
            Segment {
                kind: "user",
                hash: "0123456789abcdef".into(),
                bytes: 1
            };
            9000
        ];
        for id in ["s1", "s2", "s3", "s4"] {
            let mut r = usage(id);
            r.structure = Some(segments.clone());
            exporter.record(&r);
        }
        until("all four to arrive", || {
            seen.lock().unwrap().concat().len() == 4
        })
        .await;
        let batches = seen.lock().unwrap().clone();
        assert!(batches.len() >= 2, "{batches:?}");
        assert!(batches.iter().all(|b| b.len() <= 2), "{batches:?}");
        assert_eq!(batches.concat(), ["s1", "s2", "s3", "s4"]);
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn records_go_in_batches_of_the_size_in_order_and_a_partial_batch_goes_at_the_interval() {
        let mut server = mockito::Server::new_async().await;
        let (seen, _) = ledger(&mut server);
        let token = token("ingest-token-0123456789abc");
        let exporter = exporter(&server.url(), &token, 2, 300, 1000);
        for id in ["a1", "a2", "a3", "a4", "a5"] {
            exporter.record(&usage(id));
        }
        until("five records", || {
            seen.lock().unwrap().iter().map(Vec::len).sum::<usize>() == 5
        })
        .await;
        let batches = seen.lock().unwrap().clone();
        assert_eq!(batches.concat(), ["a1", "a2", "a3", "a4", "a5"]);
        assert!(batches.iter().all(|b| b.len() <= 2), "{batches:?}");
        assert_eq!(
            batches.last().unwrap(),
            &["a5"],
            "the odd one went at the interval"
        );
        assert_eq!(exporter.queued(), 0);
        exporter.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_control_plane_that_fails_loses_nothing_and_reorders_nothing() {
        let mut server = mockito::Server::new_async().await;
        // Two failures, then the ledger answers.
        let down = server
            .mock("POST", "/v1/usage/batch")
            .with_status(503)
            .expect(2)
            .create_async()
            .await;
        let token = token("ingest-token-0123456789abc");
        let exporter = exporter(&server.url(), &token, 10, 200, 1000);
        for id in ["b1", "b2", "b3"] {
            exporter.record(&usage(id));
        }
        until("two refusals", || {
            exporter.queued() == 3 && {
                // Both failures happen before the ledger mock below is created.
                std::thread::sleep(Duration::from_millis(50));
                true
            }
        })
        .await;
        // Let the two failures happen, then bring the ledger up.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        down.assert_async().await;
        down.remove_async().await;
        let (seen, _) = ledger(&mut server);
        until("delivery", || {
            seen.lock().unwrap().iter().map(Vec::len).sum::<usize>() == 3
        })
        .await;
        assert_eq!(seen.lock().unwrap().concat(), ["b1", "b2", "b3"]);
        exporter.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_queue_drops_the_oldest_and_recording_never_waits_for_the_network() {
        // Nothing listens, and the batch is too big and the interval too long to try meanwhile.
        let token = token("ingest-token-0123456789abc");
        let exporter = exporter("http://127.0.0.1:9", &token, 500, 60_000, 100);
        let started = Instant::now();
        for n in 0..250 {
            exporter.record(&usage(&format!("q{n:03}")));
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "recording blocked"
        );
        assert_eq!(exporter.queued(), 100);
        let kept: Vec<String> = exporter
            .shared
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|v| v["attempt_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(kept.first().unwrap(), "q150", "the oldest were dropped");
        assert_eq!(kept.last().unwrap(), "q249");
        let started = Instant::now();
        exporter.close();
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "close is bounded"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_batch_the_control_plane_calls_malformed_is_dropped_but_a_refused_credential_is_kept()
    {
        let mut server = mockito::Server::new_async().await;
        let bad = server
            .mock("POST", "/v1/usage/batch")
            .with_status(400)
            .expect(1)
            .create_async()
            .await;
        let token_ref = token("ingest-token-0123456789abc");
        let exporter = exporter(&server.url(), &token_ref, 10, 200, 1000);
        exporter.record(&usage("p1"));
        until("the batch to be tried", || exporter.queued() == 0).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        bad.assert_async().await;
        assert_eq!(
            exporter.queued(),
            0,
            "a poison batch is not retried forever"
        );
        bad.remove_async().await;
        // A 401 keeps the records: an operator can fix the token.
        let refused = server
            .mock("POST", "/v1/usage/batch")
            .with_status(401)
            .expect_at_least(1)
            .create_async()
            .await;
        exporter.record(&usage("p2"));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        refused.assert_async().await;
        assert_eq!(
            exporter.queued(),
            1,
            "the record waits for a working credential"
        );
        exporter.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_token_is_read_at_each_batch_so_a_rotation_needs_no_restart() {
        let mut server = mockito::Server::new_async().await;
        let first = server
            .mock("POST", "/v1/usage/batch")
            .match_header("authorization", "Bearer old-token-0123456789abcdef")
            .with_body(r#"{"accepted": 1, "duplicate": 0, "rejected": 0}"#)
            .expect(1)
            .create_async()
            .await;
        let token_ref = token("old-token-0123456789abcdef");
        let exporter = exporter(&server.url(), &token_ref, 1, 200, 100);
        exporter.record(&usage("t1"));
        // The queue is empty once the batch is taken, not once it is answered: wait for the request itself.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !first.matched_async().await {
            assert!(Instant::now() < deadline, "the first batch never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        first.assert_async().await;
        std::fs::write(
            token_ref.file.as_ref().unwrap(),
            "new-token-0123456789abcdef\n",
        )
        .unwrap();
        let second = server
            .mock("POST", "/v1/usage/batch")
            .match_header("authorization", "Bearer new-token-0123456789abcdef")
            .with_body(r#"{"accepted": 1, "duplicate": 0, "rejected": 0}"#)
            .expect(1)
            .create_async()
            .await;
        exporter.record(&usage("t2"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !second.matched_async().await {
            assert!(Instant::now() < deadline, "the second batch never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        second.assert_async().await;
        exporter.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_sends_what_is_queued() {
        let mut server = mockito::Server::new_async().await;
        let (seen, _) = ledger(&mut server);
        let token_ref = token("ingest-token-0123456789abc");
        // A batch too big and an interval too long to send before the close.
        let exporter = exporter(&server.url(), &token_ref, 500, 60_000, 1000);
        for id in ["c1", "c2", "c3"] {
            exporter.record(&usage(id));
        }
        let exporter = Arc::new(exporter);
        let closing = exporter.clone();
        tokio::task::spawn_blocking(move || closing.close())
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().concat(), ["c1", "c2", "c3"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_exporter_stops_its_worker_and_sends_what_it_held() {
        let mut server = mockito::Server::new_async().await;
        let (seen, _) = ledger(&mut server);
        let token_ref = token("ingest-token-0123456789abc");
        // An interval of a minute: only the drop can make the worker send these.
        let exporter = exporter(&server.url(), &token_ref, 500, 60_000, 1000);
        exporter.record(&usage("d1"));
        exporter.record(&usage("d2"));
        drop(exporter);
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().concat().len() < 2 {
            assert!(
                Instant::now() < deadline,
                "the dropped exporter never flushed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(seen.lock().unwrap().concat(), ["d1", "d2"]);
    }

    #[test]
    fn a_missing_token_or_a_bad_ca_fails_at_start() {
        let missing = SecretRef {
            env: None,
            file: Some("/nonexistent/token".into()),
        };
        fn settings(token: &SecretRef) -> ExportSettings<'_> {
            ExportSettings {
                url: "https://control.example",
                token,
                batch_size: 10,
                flush_interval: Duration::from_secs(1),
                queue_size: 100,
                ca_pem: None,
            }
        }
        assert!(UsageExporter::new(&settings(&missing), &|_| None).is_err());
        let ok = token("ingest-token-0123456789abc");
        assert!(UsageExporter::new(&settings(&ok), &|_| None).is_ok());
        let mut bad_ca = settings(&ok);
        bad_ca.ca_pem = Some(b"not a certificate");
        assert!(UsageExporter::new(&bad_ca, &|_| None).is_err());
    }
}
