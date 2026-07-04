//! Load-test harness for the proxy's rate-limit coordination layer.
//!
//! Costs $0 — it drives the real proxy against a *mock* Anthropic upstream and
//! never contacts a real provider. The mock simulates a provider rate limit by
//! returning 429 (with `Retry-After`) once concurrent in-flight requests exceed
//! a fixed capacity, the same way a real API sheds load past your TPM/RPM.
//!
//! It runs two scenarios against an identical burst load and prints a
//! comparison:
//!   A. UNCOORDINATED — no proactive limit, huge concurrency cap. The proxy
//!      forwards everything; the upstream 429s hard and the reactive retry layer
//!      absorbs it (slower, many upstream 429s).
//!   B. COORDINATED — per-instance concurrency cap sized to the upstream. The
//!      proxy sheds early with 503/429 + Retry-After, so the upstream sees almost
//!      no 429s and latency stays bounded.
//!
//! Run: cargo run --release --features proxy --example loadtest

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Json;
use serde_json::{json, Value};

// ---- Mock upstream (stands in for the Anthropic Messages API) --------------

#[derive(Default)]
struct Upstream {
    inflight: AtomicUsize,
    peak_inflight: AtomicUsize,
    total: AtomicU64,
    ok: AtomicU64,
    rate_limited: AtomicU64,
}

impl Upstream {
    fn reset(&self) {
        self.inflight.store(0, Ordering::SeqCst);
        self.peak_inflight.store(0, Ordering::SeqCst);
        self.total.store(0, Ordering::SeqCst);
        self.ok.store(0, Ordering::SeqCst);
        self.rate_limited.store(0, Ordering::SeqCst);
    }
}

struct MockCfg {
    upstream: Arc<Upstream>,
    capacity: usize,
    latency_ms: u64,
}

async fn messages(State(cfg): State<Arc<MockCfg>>, Json(_body): Json<Value>) -> impl IntoResponse {
    let up = &cfg.upstream;
    up.total.fetch_add(1, Ordering::Relaxed);
    let cur = up.inflight.fetch_add(1, Ordering::SeqCst) + 1;
    up.peak_inflight.fetch_max(cur, Ordering::SeqCst);

    if cur > cfg.capacity {
        up.inflight.fetch_sub(1, Ordering::SeqCst);
        up.rate_limited.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1")],
            "rate_limited",
        )
            .into_response();
    }

    tokio::time::sleep(Duration::from_millis(cfg.latency_ms)).await;
    up.inflight.fetch_sub(1, Ordering::SeqCst);
    up.ok.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "id": "msg_load",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }))
    .into_response()
}

async fn spawn_mock(cfg: Arc<MockCfg>) -> String {
    let app = axum::Router::new()
        .route("/messages", post(messages))
        .with_state(cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---- Proxy under test ------------------------------------------------------

async fn spawn_proxy(upstream_base: &str) -> String {
    use llmshim::providers::anthropic::Anthropic;
    use llmshim::router::Router;

    let provider = Anthropic::new("test-key".into()).with_base_url(upstream_base.to_string());
    let router = Router::new().register("anthropic", Box::new(provider));

    // AppState::from_env reads the LLMSHIM_* rate-limit vars set by the caller.
    let state = Arc::new(llmshim::proxy::AppState::from_env(router, None));
    let app = llmshim::proxy::app_with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---- Load generator --------------------------------------------------------

#[derive(Default)]
struct Report {
    ok: AtomicU64,
    rate_limited: AtomicU64, // 429 from proxy
    overloaded: AtomicU64,   // 503 from proxy
    other: AtomicU64,
    retry_after_seen: AtomicU64,
}

async fn run_load(
    proxy_base: &str,
    concurrency: usize,
    per_worker: usize,
) -> (Arc<Report>, Vec<u64>, Duration) {
    let client = reqwest::Client::new();
    let report = Arc::new(Report::default());
    let latencies = Arc::new(std::sync::Mutex::new(Vec::<u64>::with_capacity(
        concurrency * per_worker,
    )));
    let url = format!("{proxy_base}/v1/chat");
    let body = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "config": {"max_tokens": 16}
    });

    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let client = client.clone();
        let url = url.clone();
        let body = body.clone();
        let report = report.clone();
        let latencies = latencies.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..per_worker {
                let t0 = Instant::now();
                match client.post(&url).json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if resp.headers().contains_key("retry-after") {
                            report.retry_after_seen.fetch_add(1, Ordering::Relaxed);
                        }
                        match status.as_u16() {
                            200 => report.ok.fetch_add(1, Ordering::Relaxed),
                            429 => report.rate_limited.fetch_add(1, Ordering::Relaxed),
                            503 => report.overloaded.fetch_add(1, Ordering::Relaxed),
                            _ => report.other.fetch_add(1, Ordering::Relaxed),
                        };
                        let _ = resp.bytes().await;
                    }
                    Err(_) => {
                        report.other.fetch_add(1, Ordering::Relaxed);
                    }
                }
                latencies
                    .lock()
                    .unwrap()
                    .push(t0.elapsed().as_millis() as u64);
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let elapsed = start.elapsed();
    let lat = Arc::try_unwrap(latencies).unwrap().into_inner().unwrap();
    (report, lat, elapsed)
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn print_report(
    name: &str,
    up: &Upstream,
    report: &Report,
    mut latencies: Vec<u64>,
    elapsed: Duration,
) {
    latencies.sort_unstable();
    let total = report.ok.load(Ordering::Relaxed)
        + report.rate_limited.load(Ordering::Relaxed)
        + report.overloaded.load(Ordering::Relaxed)
        + report.other.load(Ordering::Relaxed);
    println!("\n=== {name} ===");
    println!("  wall time            {:>8.2}s", elapsed.as_secs_f64());
    println!("  client requests      {total:>8}");
    println!(
        "    200 ok             {:>8}",
        report.ok.load(Ordering::Relaxed)
    );
    println!(
        "    429 shed (proxy)   {:>8}",
        report.rate_limited.load(Ordering::Relaxed)
    );
    println!(
        "    503 overloaded     {:>8}",
        report.overloaded.load(Ordering::Relaxed)
    );
    println!(
        "    other/errors       {:>8}",
        report.other.load(Ordering::Relaxed)
    );
    println!(
        "    Retry-After hdr    {:>8}",
        report.retry_after_seen.load(Ordering::Relaxed)
    );
    println!(
        "  latency p50 / p99    {:>5}ms / {}ms",
        pct(&latencies, 0.50),
        pct(&latencies, 0.99)
    );
    println!("  UPSTREAM (provider):");
    println!(
        "    total hits         {:>8}",
        up.total.load(Ordering::Relaxed)
    );
    println!(
        "    200 served         {:>8}",
        up.ok.load(Ordering::Relaxed)
    );
    println!(
        "    429 rate-limited   {:>8}   <-- the number we want LOW",
        up.rate_limited.load(Ordering::Relaxed)
    );
    println!(
        "    peak concurrency   {:>8}   (mock capacity was the cap)",
        up.peak_inflight.load(Ordering::Relaxed)
    );
}

#[tokio::main]
async fn main() {
    // Keep retries modest so the uncoordinated scenario finishes promptly.
    std::env::set_var("LLMSHIM_MAX_RETRIES", "2");
    std::env::set_var("LLMSHIM_MAX_BACKOFF_SECS", "2");

    // Mock upstream: handles up to CAPACITY concurrent, 429s beyond it.
    const CAPACITY: usize = 50;
    const LATENCY_MS: u64 = 20;
    const CONCURRENCY: usize = 400;
    const PER_WORKER: usize = 5; // 2000 total requests, 400 at once

    let upstream = Arc::new(Upstream::default());
    let mock_base = spawn_mock(Arc::new(MockCfg {
        upstream: upstream.clone(),
        capacity: CAPACITY,
        latency_ms: LATENCY_MS,
    }))
    .await;

    println!(
        "Load test — mock upstream capacity={CAPACITY} concurrent, latency={LATENCY_MS}ms\n\
         Burst: {CONCURRENCY} concurrent workers x {PER_WORKER} = {} requests",
        CONCURRENCY * PER_WORKER
    );

    // --- Scenario A: UNCOORDINATED (forward everything) ---------------------
    std::env::remove_var("LLMSHIM_RATE_LIMIT_RPM");
    std::env::remove_var("LLMSHIM_RATE_LIMIT_TPM");
    std::env::set_var("LLMSHIM_MAX_CONCURRENCY", "100000");
    std::env::set_var("LLMSHIM_QUEUE_TIMEOUT_MS", "10000");
    upstream.reset();
    let proxy_a = spawn_proxy(&mock_base).await;
    let (rep_a, lat_a, el_a) = run_load(&proxy_a, CONCURRENCY, PER_WORKER).await;
    let up429_a = upstream.rate_limited.load(Ordering::Relaxed);
    let peak_a = upstream.peak_inflight.load(Ordering::Relaxed);
    print_report(
        "A. UNCOORDINATED (no proactive limit, huge concurrency cap)",
        &upstream,
        &rep_a,
        lat_a,
        el_a,
    );

    // --- Scenario B: COORDINATED (concurrency cap sized to upstream) --------
    std::env::set_var("LLMSHIM_MAX_CONCURRENCY", CAPACITY.to_string());
    std::env::set_var("LLMSHIM_QUEUE_TIMEOUT_MS", "2000");
    upstream.reset();
    let proxy_b = spawn_proxy(&mock_base).await;
    let (rep_b, lat_b, el_b) = run_load(&proxy_b, CONCURRENCY, PER_WORKER).await;
    let up429_b = upstream.rate_limited.load(Ordering::Relaxed);
    let peak_b = upstream.peak_inflight.load(Ordering::Relaxed);
    print_report(
        "B. COORDINATED (concurrency cap = upstream capacity, 2s queue)",
        &upstream,
        &rep_b,
        lat_b,
        el_b,
    );

    // --- Scenario C: RPM SHED (proactive 429 + Retry-After) -----------------
    // Global RPM budget of 300 with a burst of 2000 -> most requests must be
    // proactively rejected with 429 + Retry-After (never touching upstream).
    std::env::set_var("LLMSHIM_MAX_CONCURRENCY", CAPACITY.to_string());
    std::env::set_var("LLMSHIM_QUEUE_TIMEOUT_MS", "2000");
    std::env::set_var("LLMSHIM_RATE_LIMIT_RPM", "300");
    upstream.reset();
    let proxy_c = spawn_proxy(&mock_base).await;
    let (rep_c, lat_c, el_c) = run_load(&proxy_c, CONCURRENCY, PER_WORKER).await;
    print_report(
        "C. RPM SHED (LLMSHIM_RATE_LIMIT_RPM=300, burst 2000)",
        &upstream,
        &rep_c,
        lat_c,
        el_c,
    );
    let shed_429_c = rep_c.rate_limited.load(Ordering::Relaxed);
    let ra_c = rep_c.retry_after_seen.load(Ordering::Relaxed);
    std::env::remove_var("LLMSHIM_RATE_LIMIT_RPM");

    // --- Scenario D: OVERLOAD (bounded queue -> 503 + Retry-After) ----------
    // Tiny concurrency cap + short queue timeout under heavy burst -> the queue
    // times out and sheds excess as 503 + Retry-After instead of hanging.
    std::env::set_var("LLMSHIM_MAX_CONCURRENCY", "5");
    std::env::set_var("LLMSHIM_QUEUE_TIMEOUT_MS", "200");
    upstream.reset();
    let proxy_d = spawn_proxy(&mock_base).await;
    let (rep_d, lat_d, el_d) = run_load(&proxy_d, CONCURRENCY, PER_WORKER).await;
    print_report(
        "D. OVERLOAD (MAX_CONCURRENCY=5, 200ms queue, burst 2000)",
        &upstream,
        &rep_d,
        lat_d,
        el_d,
    );
    let over_503_d = rep_d.overloaded.load(Ordering::Relaxed);
    let ra_d = rep_d.retry_after_seen.load(Ordering::Relaxed);

    // --- Verdict ------------------------------------------------------------
    println!("\n=== VERDICT ===");
    println!("  upstream 429s:       uncoordinated={up429_a:<6}  coordinated={up429_b}");
    println!("  upstream peak conc.: uncoordinated={peak_a:<6}  coordinated={peak_b}  (cap was {CAPACITY})");
    println!(
        "  clients served ok:   uncoordinated={:<6}  coordinated={}",
        rep_a.ok.load(Ordering::Relaxed),
        rep_b.ok.load(Ordering::Relaxed)
    );
    println!(
        "  wall time:           uncoordinated={:.2}s   coordinated={:.2}s",
        el_a.as_secs_f64(),
        el_b.as_secs_f64()
    );
    println!(
        "\n  Coordinated mode should keep upstream peak concurrency at/under the cap ({CAPACITY})\n  \
         and drive upstream 429s toward zero — protecting the provider — while shedding\n  \
         excess as client-visible 503/429 + Retry-After instead of hammering upstream."
    );

    // --- Path-coverage checks (fail loudly if a shed path misbehaves) -------
    println!("\n=== PATH CHECKS ===");
    let mut all_ok = true;
    let mut check = |name: &str, cond: bool| {
        println!("  [{}] {name}", if cond { "PASS" } else { "FAIL" });
        all_ok &= cond;
    };
    check("coordinated: zero upstream 429s", up429_b == 0);
    check("coordinated: upstream peak <= cap", peak_b <= CAPACITY);
    check("RPM shed: emitted 429s to clients", shed_429_c > 0);
    check(
        "RPM shed: every 429 carried Retry-After",
        ra_c >= shed_429_c && shed_429_c > 0,
    );
    check("overload: emitted 503s to clients", over_503_d > 0);
    check(
        "overload: every 503 carried Retry-After",
        ra_d >= over_503_d && over_503_d > 0,
    );
    check(
        "no requests hung or errored across all scenarios",
        rep_a.other.load(Ordering::Relaxed) == 0
            && rep_b.other.load(Ordering::Relaxed) == 0
            && rep_c.other.load(Ordering::Relaxed) == 0
            && rep_d.other.load(Ordering::Relaxed) == 0,
    );
    println!(
        "\n  {}",
        if all_ok {
            "ALL PATH CHECKS PASSED"
        } else {
            "SOME CHECKS FAILED — see above"
        }
    );
    std::process::exit(if all_ok { 0 } else { 1 });
}
