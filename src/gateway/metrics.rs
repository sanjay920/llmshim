//! Lightweight, dependency-free Prometheus metrics for the gateway.
//!
//! A process-global registry of counters, gauges, and latency histograms,
//! exported in Prometheus text format at `GET /metrics`. Hand-rolled to avoid a
//! metrics-crate dependency; label cardinality is kept low on purpose (provider,
//! tier, outcome — never per-tenant/per-request).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

// Metric names shared by the in-memory scheduler and the distributed worker.
/// Requests accepted onto the queue (labels: provider, tier, mode).
pub const REQUESTS: &str = "llmshim_gateway_requests_total";
/// Requests dispatched to the upstream successfully (label: provider).
pub const DISPATCHED: &str = "llmshim_gateway_dispatched_total";
/// Requests that did not dispatch (labels: provider, reason).
pub const REJECTED: &str = "llmshim_gateway_rejected_total";
/// In-flight upstream calls (gauge, label: provider).
pub const INFLIGHT: &str = "llmshim_gateway_inflight";
/// Time a job waited in the queue before dispatch (histogram, label: provider).
pub const QUEUE_WAIT: &str = "llmshim_gateway_queue_wait_ms";
/// Upstream call latency (histogram, label: provider).
pub const UPSTREAM_LATENCY: &str = "llmshim_gateway_upstream_latency_ms";
/// Current queue depth (gauge, label: provider) — set at scrape time.
pub const QUEUE_DEPTH: &str = "llmshim_gateway_queue_depth";

/// Upper bounds (ms) for latency histograms — the Prometheus `le` buckets.
const BUCKETS_MS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0,
    60000.0,
];

struct Histogram {
    /// One counter per `BUCKETS_MS` bound, plus `+Inf` is `count`.
    buckets: Vec<AtomicU64>,
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: (0..BUCKETS_MS.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
    fn observe(&self, ms: f64) {
        for (i, &bound) in BUCKETS_MS.iter().enumerate() {
            if ms <= bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.sum_ms.fetch_add(ms.max(0.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Process-global metrics registry.
pub struct Metrics {
    counters: RwLock<BTreeMap<String, AtomicU64>>,
    gauges: RwLock<BTreeMap<String, AtomicI64>>,
    histos: RwLock<BTreeMap<String, Histogram>>,
}

static REGISTRY: OnceLock<Metrics> = OnceLock::new();

/// The global registry (created on first use).
pub fn registry() -> &'static Metrics {
    REGISTRY.get_or_init(|| Metrics {
        counters: RwLock::new(BTreeMap::new()),
        gauges: RwLock::new(BTreeMap::new()),
        histos: RwLock::new(BTreeMap::new()),
    })
}

/// `name{k="v",...}` with labels sorted for a stable series key.
fn series(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let mut sorted: Vec<&(&str, &str)> = labels.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut s = String::with_capacity(name.len() + 16);
    s.push_str(name);
    s.push('{');
    for (i, (k, v)) in sorted.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{k}=\"{v}\"");
    }
    s.push('}');
    s
}

/// Increment a counter by 1.
pub fn incr(name: &str, labels: &[(&str, &str)]) {
    add(name, labels, 1);
}

/// Add to a counter.
pub fn add(name: &str, labels: &[(&str, &str)], n: u64) {
    let key = series(name, labels);
    let reg = registry();
    if let Some(c) = reg.counters.read().unwrap().get(&key) {
        c.fetch_add(n, Ordering::Relaxed);
        return;
    }
    reg.counters
        .write()
        .unwrap()
        .entry(key)
        .or_insert_with(|| AtomicU64::new(0))
        .fetch_add(n, Ordering::Relaxed);
}

/// Adjust a gauge by a signed delta.
pub fn gauge_add(name: &str, labels: &[(&str, &str)], delta: i64) {
    let key = series(name, labels);
    let reg = registry();
    if let Some(g) = reg.gauges.read().unwrap().get(&key) {
        g.fetch_add(delta, Ordering::Relaxed);
        return;
    }
    reg.gauges
        .write()
        .unwrap()
        .entry(key)
        .or_insert_with(|| AtomicI64::new(0))
        .fetch_add(delta, Ordering::Relaxed);
}

/// Set a gauge to an absolute value.
pub fn gauge_set(name: &str, labels: &[(&str, &str)], value: i64) {
    let key = series(name, labels);
    let reg = registry();
    if let Some(g) = reg.gauges.read().unwrap().get(&key) {
        g.store(value, Ordering::Relaxed);
        return;
    }
    reg.gauges
        .write()
        .unwrap()
        .entry(key)
        .or_insert_with(|| AtomicI64::new(0))
        .store(value, Ordering::Relaxed);
}

/// Increments the [`INFLIGHT`] gauge for `provider` and decrements it on drop,
/// so the count is correct across every exit path (early return, error, panic).
pub struct InflightGuard {
    provider: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        gauge_add(INFLIGHT, &[("provider", &self.provider)], -1);
    }
}

/// Mark one in-flight upstream call for `provider` until the returned guard drops.
pub fn inflight(provider: &str) -> InflightGuard {
    gauge_add(INFLIGHT, &[("provider", provider)], 1);
    InflightGuard {
        provider: provider.to_string(),
    }
}

/// Record a latency observation (milliseconds).
pub fn observe_ms(name: &str, labels: &[(&str, &str)], ms: f64) {
    let key = series(name, labels);
    let reg = registry();
    if let Some(h) = reg.histos.read().unwrap().get(&key) {
        h.observe(ms);
        return;
    }
    let mut w = reg.histos.write().unwrap();
    w.entry(key).or_insert_with(Histogram::new).observe(ms);
}

/// Base metric name (strip the `{labels}` suffix) — used to group series and
/// emit one `# TYPE` line per family.
fn base(series_key: &str) -> &str {
    match series_key.find('{') {
        Some(i) => &series_key[..i],
        None => series_key,
    }
}

/// Render the whole registry in Prometheus text exposition format.
pub fn render() -> String {
    let reg = registry();
    let mut out = String::new();

    let counters = reg.counters.read().unwrap();
    let mut last = "";
    for (key, val) in counters.iter() {
        let b = base(key);
        if b != last {
            let _ = writeln!(out, "# TYPE {b} counter");
            last = b;
        }
        let _ = writeln!(out, "{key} {}", val.load(Ordering::Relaxed));
    }

    let gauges = reg.gauges.read().unwrap();
    last = "";
    for (key, val) in gauges.iter() {
        let b = base(key);
        if b != last {
            let _ = writeln!(out, "# TYPE {b} gauge");
            last = b;
        }
        let _ = writeln!(out, "{key} {}", val.load(Ordering::Relaxed));
    }

    let histos = reg.histos.read().unwrap();
    for (key, h) in histos.iter() {
        let b = base(key);
        let _ = writeln!(out, "# TYPE {b} histogram");
        let (name, labels) = match key.split_once('{') {
            Some((n, rest)) => (n, Some(rest.trim_end_matches('}'))),
            None => (key.as_str(), None),
        };
        let mut cumulative = 0u64;
        for (i, &bound) in BUCKETS_MS.iter().enumerate() {
            cumulative = h.buckets[i].load(Ordering::Relaxed);
            match labels {
                Some(l) => {
                    let _ = writeln!(out, "{name}_bucket{{{l},le=\"{bound}\"}} {cumulative}");
                }
                None => {
                    let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
                }
            }
        }
        let count = h.count.load(Ordering::Relaxed);
        let _ = cumulative; // last bucket value not reused
        match labels {
            Some(l) => {
                let _ = writeln!(out, "{name}_bucket{{{l},le=\"+Inf\"}} {count}");
                let _ = writeln!(
                    out,
                    "{name}_sum{{{l}}} {}",
                    h.sum_ms.load(Ordering::Relaxed)
                );
                let _ = writeln!(out, "{name}_count{{{l}}} {count}");
            }
            None => {
                let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
                let _ = writeln!(out, "{name}_sum {}", h.sum_ms.load(Ordering::Relaxed));
                let _ = writeln!(out, "{name}_count {count}");
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_key_is_stable_and_sorted() {
        assert_eq!(series("m", &[]), "m");
        assert_eq!(series("m", &[("b", "2"), ("a", "1")]), "m{a=\"1\",b=\"2\"}");
    }

    #[test]
    fn counters_gauges_histograms_render() {
        // Use names unique to this test so the shared registry stays clean.
        incr("t_reqs_total", &[("provider", "openai")]);
        add("t_reqs_total", &[("provider", "openai")], 2);
        gauge_add("t_inflight", &[("provider", "openai")], 3);
        gauge_add("t_inflight", &[("provider", "openai")], -1);
        observe_ms("t_wait_ms", &[("provider", "openai")], 42.0);

        let out = render();
        assert!(out.contains("# TYPE t_reqs_total counter"));
        assert!(out.contains("t_reqs_total{provider=\"openai\"} 3"));
        assert!(out.contains("# TYPE t_inflight gauge"));
        assert!(out.contains("t_inflight{provider=\"openai\"} 2"));
        assert!(out.contains("# TYPE t_wait_ms histogram"));
        assert!(out.contains("t_wait_ms_count{provider=\"openai\"} 1"));
        assert!(out.contains("t_wait_ms_bucket{provider=\"openai\",le=\"50\"} 1"));
    }
}
