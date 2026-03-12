//! Benchmark: llmshim (Rust) vs litellm/langchain (Python)
//!
//! Run:
//!   cargo run --release --example bench
//!   python3 benchmarks/bench_python.py

use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Instant;

const MODEL_ANTHROPIC: &str = "anthropic/claude-sonnet-4-6";
const MODEL_OPENAI: &str = "openai/gpt-5.4";
const PROMPT: &str = "Say 'benchmark' and nothing else.";
const MAX_TOKENS: u32 = 50;
const WARM_RUNS: usize = 20;

fn fmt_ms(ms: f64) -> String {
    format!("{ms:.0}ms")
}

fn section(name: &str) {
    println!("\n{}", "=".repeat(60));
    println!("  {name}");
    println!("{}", "=".repeat(60));
}

fn p50(times: &[f64]) -> f64 {
    let mut sorted = times.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted[sorted.len() / 2]
}

fn make_request(model: &str) -> Value {
    let mut req = json!({
        "model": model,
        "messages": [{"role": "user", "content": PROMPT}],
        "max_tokens": MAX_TOKENS,
    });
    if model.contains("anthropic") || model.contains("claude") {
        req["x-anthropic"] = json!({"disable_1m_context": true});
    }
    req
}

fn get_memory_mb() -> f64 {
    use std::process::Command;
    let pid = std::process::id();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok();
    if let Some(out) = output {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Ok(kb) = s.trim().parse::<f64>() {
            return kb / 1024.0;
        }
    }
    0.0
}

#[tokio::main]
async fn main() {
    let router = llmshim::router::Router::from_env();
    let mut results: Vec<(&str, f64)> = Vec::new();

    // Warmup — pre-establish TCP+TLS connections
    section("Connection Warmup");
    let t0 = Instant::now();
    llmshim::warmup(&router).await;
    println!(
        "  TCP+TLS pre-connect: {}",
        fmt_ms(t0.elapsed().as_secs_f64() * 1000.0)
    );

    // First request
    section("First Request (Anthropic)");
    let req = make_request(MODEL_ANTHROPIC);
    let t0 = Instant::now();
    let _ = llmshim::completion(&router, &req).await.unwrap();
    let first_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("  llmshim:  {}", fmt_ms(first_ms));
    results.push(("first_request_anthropic_ms", first_ms));

    // Warm requests (Anthropic)
    section(&format!("Warm Requests (Anthropic) — {WARM_RUNS} runs"));
    let mut times = Vec::new();
    for _ in 0..WARM_RUNS {
        let t0 = Instant::now();
        let _ = llmshim::completion(&router, &req).await.unwrap();
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let p50_ant = p50(&times);
    println!(
        "  llmshim:  p50={}  avg={}",
        fmt_ms(p50_ant),
        fmt_ms(times.iter().sum::<f64>() / times.len() as f64)
    );
    results.push(("warm_anthropic_p50_ms", p50_ant));

    // Warm requests (OpenAI)
    section(&format!("Warm Requests (OpenAI) — {WARM_RUNS} runs"));
    let req_oai = make_request(MODEL_OPENAI);
    let _ = llmshim::completion(&router, &req_oai).await; // warmup

    let mut times = Vec::new();
    for i in 0..WARM_RUNS {
        let t0 = Instant::now();
        match llmshim::completion(&router, &req_oai).await {
            Ok(_) => times.push(t0.elapsed().as_secs_f64() * 1000.0),
            Err(e) => println!("  run {}: error — {e}", i + 1),
        }
    }
    if times.is_empty() {
        println!("  all OpenAI requests failed");
        results.push(("warm_openai_p50_ms", 0.0));
    } else {
        let p50_oai = p50(&times);
        println!(
            "  llmshim:  p50={}  avg={}  ({}/{WARM_RUNS} ok)",
            fmt_ms(p50_oai),
            fmt_ms(times.iter().sum::<f64>() / times.len() as f64),
            times.len()
        );
        results.push(("warm_openai_p50_ms", p50_oai));
    }

    // Streaming TTFT
    section("Streaming — Time to First Token (Anthropic)");
    let t0 = Instant::now();
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut ttft = None;
    while let Some(Ok(chunk)) = stream.next().await {
        if let Ok(parsed) = serde_json::from_str::<Value>(&chunk) {
            let content = parsed
                .pointer("/choices/0/delta/content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !content.is_empty() {
                ttft = Some(t0.elapsed().as_secs_f64() * 1000.0);
                break;
            }
        }
    }
    let ttft_ms = ttft.unwrap_or(0.0);
    println!("  llmshim:  {}", fmt_ms(ttft_ms));
    results.push(("ttft_anthropic_ms", ttft_ms));

    // Memory
    let mem = get_memory_mb();
    results.push(("memory_rss_mb", mem));

    // Transform overhead
    section("Transform Overhead");
    let (provider, model_name) = router.resolve(MODEL_ANTHROPIC).unwrap();
    let iterations = 10_000;
    let t0 = Instant::now();
    for _ in 0..iterations {
        let _ = provider.transform_request(&model_name, &req);
    }
    let transform_us = t0.elapsed().as_micros() as f64 / iterations as f64;
    println!("  {transform_us:.1}µs per request transform ({iterations} iterations)");

    // Summary
    section("SUMMARY");
    println!("  {:<35} {:>12}", "Metric", "llmshim");
    println!("  {} {}", "-".repeat(35), "-".repeat(12));
    for (metric, val) in &results {
        let label = metric.replace('_', " ");
        if *val == 0.0 {
            println!("  {label:<35} {:>12}", "skipped");
        } else if label.contains("mb") {
            println!("  {label:<35} {:>10.1}MB", val);
        } else {
            println!("  {label:<35} {:>10}ms", *val as u64);
        }
    }
    println!("  {:<35} {:>10.1}µs", "transform overhead", transform_us);
}
