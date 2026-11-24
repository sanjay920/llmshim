//! Release benchmark: actual API latency and local request-transform cost.
//! cargo run --release --example bench
//! cargo run --release --example bench -- --transforms-only
//! LLMSHIM_NO_SCHEMA_CACHE=1 cargo run --release --example bench -- --transforms-only
use futures::StreamExt;
use llmshim::{
    provider::Provider,
    providers::{anthropic::Anthropic, openai::OpenAi},
    router::Router,
};
use serde_json::{json, Value};
use std::{hint::black_box, time::Instant};

const MODEL_ANTHROPIC: &str = "anthropic/claude-sonnet-4-6";
const MODEL_OPENAI: &str = "openai/gpt-5.4";
const PROMPT: &str = "Say 'benchmark' and nothing else.";
const MAX_TOKENS: u32 = 50;
const WARM_RUNS: usize = 20;
const ITERATIONS: usize = 10_000;

fn section(name: &str) {
    println!("\n{}\n  {name}\n{}", "=".repeat(60), "=".repeat(60));
}
fn p50(times: &[f64]) -> f64 {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}
fn make_request(model: &str) -> Value {
    let mut request = json!({"model":model,"messages":[{"role":"user","content":PROMPT}],"max_tokens":MAX_TOKENS,"reasoning_effort":"none"});
    if model.starts_with("anthropic/") {
        request["x-anthropic"] = json!({"disable_1m_context":true});
    }
    request
}
fn with_tools(count: usize) -> Value {
    let mut request = make_request(MODEL_ANTHROPIC);
    if count > 0 {
        request["tools"]=json!((0..count).map(|index|json!({
            "type":"function","function":{
                "name":format!("lookup_{index}"),"description":"Look up a record.",
                "parameters":{"type":"object","description":format!("Input for lookup {index}"),
                    "properties":{"key":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":10}},
                    "required":["key"],"additionalProperties":false}
            }
        })).collect::<Vec<_>>());
    }
    request
}
fn transforms(provider: &dyn Provider, model: &str) -> Result<(), String> {
    section("Local request transforms — no HTTP calls");
    let disabled = std::env::var("LLMSHIM_NO_SCHEMA_CACHE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    println!(
        "  Schema memoization: {}",
        if disabled { "disabled" } else { "enabled" }
    );
    println!("  Distinct parameter schemas, reused across {ITERATIONS} iterations.");
    println!(
        "  Includes request copies, replay/cache policy, schema handling and native translation."
    );
    println!("  Excludes HTTP serialization/dispatch and the higher-level capability plan.");
    // Initialize the resident catalog outside the measured loops.
    provider
        .transform_request(model, &with_tools(0))
        .map_err(|_| "could not prepare transform fixture")?;
    println!("  {:>6} {:>16}", "tools", "mean µs/request");
    for count in [0, 1, 5, 10, 25, 50] {
        let request = with_tools(count);
        provider
            .transform_request(model, &request)
            .map_err(|_| "invalid transform fixture")?;
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                provider
                    .transform_request(black_box(model), black_box(&request))
                    .map_err(|_| "request transform failed")?,
            );
        }
        let micros = start.elapsed().as_secs_f64() * 1_000_000.0 / ITERATIONS as f64;
        println!("  {count:>6} {micros:>16.2}");
    }
    Ok(())
}
fn memory_mb() -> Option<f64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8(output.stdout)
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?
            / 1024.0,
    )
}
fn failure(stage: &str, error: llmshim::error::ShimError) -> String {
    // Provider errors can echo credentials. Publish only status and stage.
    match error {
        llmshim::error::ShimError::ProviderError { status, .. } => format!(
            "{stage}: provider HTTP {status}; check the corresponding API key and model access"
        ),
        _ => format!("{stage}: request/transport failed; no latency summary was recorded"),
    }
}
async fn completion(router: &Router, request: &Value, stage: &str) -> Result<f64, String> {
    let start = Instant::now();
    let result = llmshim::completion(router, request)
        .await
        .map_err(|error| failure(stage, error))?;
    let millis = start.elapsed().as_secs_f64() * 1000.0;
    if result["choices"][0]["finish_reason"] != "stop"
        || result["choices"][0]["message"]["content"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(format!(
            "{stage}: incomplete or empty response; no latency summary was recorded"
        ));
    }
    Ok(millis)
}
async fn warm(router: &Router, request: &Value, label: &str) -> Result<(f64, f64), String> {
    section(&format!("Warm Requests ({label}) — {WARM_RUNS} runs"));
    let mut times = Vec::with_capacity(WARM_RUNS);
    for run in 1..=WARM_RUNS {
        times.push(completion(router, request, label).await?);
        if run % 5 == 0 {
            println!("  {run}/{WARM_RUNS} complete");
        }
    }
    let median = p50(&times);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!("  p50={median:.2}ms  mean={mean:.2}ms  ({WARM_RUNS}/{WARM_RUNS} complete)");
    Ok((median, mean))
}
fn required_key(name: &str) -> Result<String, String> {
    std::env::var(name).ok().filter(|key|!key.trim().is_empty())
        .ok_or_else(||format!("set {name} or configure it with `llmshim configure`; use --transforms-only for a benchmark without API keys"))
}
async fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] || args == ["-h"] {
        println!("Usage: cargo run --release --example bench [-- --transforms-only]\n\nDefault: 43 logical requests using ANTHROPIC_API_KEY and OPENAI_API_KEY (or saved config); client retries may add attempts.\n--transforms-only: local CPU measurement; no keys or HTTP requests.\nLLMSHIM_NO_SCHEMA_CACHE=1 disables memoization for comparison.");
        return Ok(());
    }
    if !args.is_empty() && args != ["--transforms-only"] {
        return Err("unknown benchmark argument; use --help".into());
    }
    println!(
        "llmshim {} · {} · {}/{}",
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().to_rfc3339(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if args == ["--transforms-only"] {
        return transforms(&Anthropic::new(String::new()), "claude-sonnet-4-6");
    }
    llmshim::env::load_all();
    let anthropic = required_key("ANTHROPIC_API_KEY")?;
    let openai = required_key("OPENAI_API_KEY")?;
    let router = Router::new()
        .register("anthropic", Box::new(Anthropic::new(anthropic)))
        .register("openai", Box::new(OpenAi::new(openai)));
    println!("Models: {MODEL_ANTHROPIC}, {MODEL_OPENAI}");
    println!("Prompt: {PROMPT:?}; max_tokens={MAX_TOKENS}; reasoning_effort=none.");
    section("Connection Warmup");
    let start = Instant::now();
    llmshim::warmup(&router).await;
    println!(
        "  TCP+TLS pre-connect: {:.2}ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let request = make_request(MODEL_ANTHROPIC);
    section("First Request (Anthropic, after connection warmup)");
    let first = completion(&router, &request, "Anthropic first request").await?;
    println!("  {first:.2}ms");
    let (anthropic_median, anthropic_mean) = warm(&router, &request, "Anthropic").await?;
    let openai_request = make_request(MODEL_OPENAI);
    completion(&router, &openai_request, "OpenAI warmup").await?;
    let (openai_median, openai_mean) = warm(&router, &openai_request, "OpenAI").await?;
    section("Streaming — Time to First Visible Text (Anthropic)");
    let start = Instant::now();
    let mut stream = llmshim::stream(&router, &request)
        .await
        .map_err(|error| failure("Anthropic stream", error))?;
    let mut ttft = None;
    let mut complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| failure("Anthropic stream", error))?;
        let parsed: Value =
            serde_json::from_str(&chunk).map_err(|_| "invalid normalized stream JSON")?;
        if ttft.is_none()
            && parsed
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
        {
            ttft = Some(start.elapsed().as_secs_f64() * 1000.0);
        }
        if let Some(finish) = parsed
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            complete = finish == "stop";
        }
    }
    let ttft = ttft
        .filter(|_| complete)
        .ok_or("Anthropic stream was incomplete or had no visible text")?;
    println!("  {ttft:.2}ms");
    let rss = memory_mb();
    section("SUMMARY — one run, successful responses only");
    println!("  Anthropic first request: {first:.2}ms");
    println!(
        "  Anthropic warm p50: {anthropic_median:.2}ms; mean: {anthropic_mean:.2}ms; n={WARM_RUNS}"
    );
    println!("  OpenAI warm p50: {openai_median:.2}ms; mean: {openai_mean:.2}ms; n={WARM_RUNS}");
    println!("  Anthropic streaming TTFT: {ttft:.2}ms; n=1");
    if let Some(rss) = rss {
        println!("  RSS after API calls, before tool sweep: {rss:.2}MiB");
    }
    let (provider, model) = router
        .resolve(MODEL_ANTHROPIC)
        .map_err(|_| "Anthropic route unavailable after preflight")?;
    transforms(provider, &model)
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("benchmark: {error}");
        std::process::exit(1);
    }
}
