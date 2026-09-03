//! Gateway load test — drives the in-process priority [`Scheduler`] with tens of
//! thousands of concurrent mixed-priority requests against a fake (no-network)
//! dispatcher, so it exercises the scheduler itself, not any provider.
//!
//! Run: `cargo run --release --features gateway --example gateway_loadtest`
//!
//! Reports throughput + latency percentiles and asserts two invariants:
//!   1. **Zero loss** under a burst far larger than the concurrency limit.
//!   2. **Priority holds under load** — high-tier p50 latency ≪ low-tier p50
//!      when the concurrency slots are saturated.
//! Exits non-zero if either invariant fails.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use llmshim::gateway::{Dispatch, DispatchError, GatewayConfig, GatewayRequest, Scheduler};
use llmshim::proxy::ratelimit::{InMemoryRateLimiter, RateLimitConfig};
use serde_json::{json, Value};

/// A no-network dispatcher with a fixed simulated latency.
struct FakeDispatch {
    latency: Duration,
    served: Arc<AtomicU64>,
}

#[async_trait]
impl Dispatch for FakeDispatch {
    async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        self.served.fetch_add(1, Ordering::Relaxed);
        Ok(payload)
    }
}

fn unlimited() -> Arc<InMemoryRateLimiter> {
    Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default()))
}

fn percentile(sorted_us: &[u128], p: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() as f64 - 1.0) * p).round() as usize;
    sorted_us[idx] as f64 / 1000.0 // → ms
}

fn report(name: &str, mut latencies_us: Vec<u128>, wall: Duration) {
    latencies_us.sort_unstable();
    let n = latencies_us.len();
    let rps = n as f64 / wall.as_secs_f64();
    println!(
        "{name}: {n} reqs in {:.2}s = {rps:.0} req/s | p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        wall.as_secs_f64(),
        percentile(&latencies_us, 0.50),
        percentile(&latencies_us, 0.95),
        percentile(&latencies_us, 0.99),
        percentile(&latencies_us, 1.00),
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut failures = 0;

    // ---- Scenario A: throughput + zero loss under a large burst ------------
    {
        let served = Arc::new(AtomicU64::new(0));
        let config = GatewayConfig {
            max_queue_depth: 500_000,
            max_concurrency_per_provider: 512,
            max_wait: Duration::from_secs(60),
            ..Default::default()
        };
        let sched = Scheduler::new(
            config,
            unlimited(),
            Arc::new(FakeDispatch {
                latency: Duration::from_millis(2),
                served: served.clone(),
            }),
        );

        let n = 20_000u64;
        let ok = Arc::new(AtomicU64::new(0));
        let errs = Arc::new(AtomicU64::new(0));
        let start = Instant::now();
        let mut handles = Vec::with_capacity(n as usize);
        for i in 0..n {
            let s = sched.clone();
            let ok = ok.clone();
            let errs = errs.clone();
            handles.push(tokio::spawn(async move {
                let t = Instant::now();
                let r = s
                    .submit(GatewayRequest {
                        provider: "bench".into(),
                        tier: (i % 3) as u8,
                        permits: 1,
                        payload: json!({ "id": i }),
                    })
                    .await;
                match r {
                    Ok(_) => ok.fetch_add(1, Ordering::Relaxed),
                    Err(_) => errs.fetch_add(1, Ordering::Relaxed),
                };
                t.elapsed().as_micros()
            }));
        }
        let mut lat = Vec::with_capacity(n as usize);
        for h in handles {
            lat.push(h.await.unwrap());
        }
        let wall = start.elapsed();
        report("throughput", lat, wall);
        let (ok, errs) = (ok.load(Ordering::Relaxed), errs.load(Ordering::Relaxed));
        println!(
            "  ok={ok} errors={errs} served={}",
            served.load(Ordering::Relaxed)
        );
        if errs != 0 || ok != n {
            eprintln!("  FAIL: expected {n} successful, zero lost — got ok={ok} errs={errs}");
            failures += 1;
        } else {
            println!("  PASS: zero loss under burst");
        }
    }

    // ---- Scenario B: priority holds when concurrency is saturated ----------
    {
        let served = Arc::new(AtomicU64::new(0));
        let config = GatewayConfig {
            max_queue_depth: 100_000,
            max_concurrency_per_provider: 8, // the bottleneck
            max_wait: Duration::from_secs(120),
            ..Default::default()
        };
        let sched = Scheduler::new(
            config,
            unlimited(),
            Arc::new(FakeDispatch {
                latency: Duration::from_millis(25),
                served: served.clone(),
            }),
        );

        let per_tier = 800u64;
        let low_lat = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
        let high_lat = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
        let start = Instant::now();
        let mut handles = Vec::new();
        // Interleave low- and high-tier submissions.
        for i in 0..per_tier {
            for (tier, bucket) in [(0u8, &low_lat), (5u8, &high_lat)] {
                let s = sched.clone();
                let bucket = bucket.clone();
                handles.push(tokio::spawn(async move {
                    let t = Instant::now();
                    let _ = s
                        .submit(GatewayRequest {
                            provider: "bench".into(),
                            tier,
                            permits: 1,
                            payload: json!({ "id": i }),
                        })
                        .await;
                    bucket.lock().unwrap().push(t.elapsed().as_micros());
                }));
            }
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = start.elapsed();
        let low = Arc::try_unwrap(low_lat).unwrap().into_inner().unwrap();
        let high = Arc::try_unwrap(high_lat).unwrap().into_inner().unwrap();
        report("priority(low tier)", low.clone(), wall);
        report("priority(high tier)", high.clone(), wall);
        let mut lo = low;
        let mut hi = high;
        lo.sort_unstable();
        hi.sort_unstable();
        let low_p50 = percentile(&lo, 0.50);
        let high_p50 = percentile(&hi, 0.50);
        if high_p50 < low_p50 {
            println!("  PASS: high-tier p50 {high_p50:.1}ms < low-tier p50 {low_p50:.1}ms");
        } else {
            eprintln!(
                "  FAIL: priority not honored — high p50 {high_p50:.1}ms >= low p50 {low_p50:.1}ms"
            );
            failures += 1;
        }
    }

    if failures > 0 {
        eprintln!("\n{failures} scenario(s) FAILED");
        std::process::exit(1);
    }
    println!("\nAll load-test invariants held.");
}
