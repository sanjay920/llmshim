//! Reproducible benchmark of `llmshim serve` against a mock upstream (0018).
//!
//! It measures what the gateway adds, not what a provider costs: a local mock
//! answers instantly (or after `--think-ms`), the same closed-loop client load is
//! sent once straight at the mock and once through a real `llmshim serve`
//! process, and the report gives added latency (p50, p99), throughput and the
//! process's resident memory, with the method, host, build and date. It costs $0
//! and contacts no provider.
//!
//! Build the binary and the example in release mode first; a debug build is
//! refused, because its numbers would mean nothing:
//!
//! ```text
//! cargo build --release --features serve
//! cargo run --release --features serve --example serve_bench -- --json out.json
//! ```
//!
//! The client, the mock and the gateway share the machine, so absolute figures
//! include that contention; the *difference* between the two phases is the
//! number to read, and the host is printed so a reader can judge.

use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEY: &str = "bench-caller-key-0123456789abcdef";
const UPSTREAM_KEY: &str = "bench-upstream-key-0123456789abcdef";

struct Args {
    levels: Vec<usize>,
    duration: Duration,
    warmup: Duration,
    think: Duration,
    binary: String,
    json: Option<String>,
}

fn parse() -> Args {
    let mut args = Args {
        levels: vec![1, 8, 32],
        duration: Duration::from_secs(15),
        warmup: Duration::from_secs(3),
        think: Duration::ZERO,
        binary: "target/release/llmshim".into(),
        json: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--concurrency" => {
                args.levels = value()
                    .split(',')
                    .map(|n| n.trim().parse().ok().filter(|n| *n > 0))
                    .collect::<Option<Vec<usize>>>()
                    .filter(|levels| !levels.is_empty())
                    .unwrap_or_else(|| usage("--concurrency takes numbers like 1,8,32"))
            }
            "--duration" => {
                args.duration =
                    Duration::from_secs(value().parse().unwrap_or_else(|_| usage("bad --duration")))
            }
            "--warmup" => {
                args.warmup =
                    Duration::from_secs(value().parse().unwrap_or_else(|_| usage("bad --warmup")))
            }
            "--think-ms" => {
                args.think = Duration::from_millis(
                    value().parse().unwrap_or_else(|_| usage("bad --think-ms")),
                )
            }
            "--bin" => args.binary = value(),
            "--json" => args.json = Some(value()),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    args
}

fn usage(problem: &str) -> ! {
    if !problem.is_empty() {
        eprintln!("error: {problem}\n");
    }
    eprintln!(
        "usage: serve_bench [--concurrency N,N,...] [--duration SECS] [--warmup SECS] \
         [--think-ms MS] [--bin PATH] [--json FILE]"
    );
    std::process::exit(if problem.is_empty() { 0 } else { 2 });
}

fn completion() -> String {
    json!({"id": "bench", "object": "chat.completion", "created": 1, "model": "bench-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}})
    .to_string()
}

async fn mock(think: Duration) -> String {
    let body = Arc::new(completion());
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let body = body.clone();
            async move {
                if !think.is_zero() {
                    tokio::time::sleep(think).await;
                }
                ([("content-type", "application/json")], (*body).clone())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

struct Phase {
    name: &'static str,
    concurrency: usize,
    failures: std::collections::BTreeMap<u16, u64>,
    requests: u64,
    errors: u64,
    seconds: f64,
    p50_us: u64,
    p99_us: u64,
    mean_us: u64,
}

impl Phase {
    fn rps(&self) -> f64 {
        self.requests as f64 / self.seconds
    }
}

async fn drive(
    name: &'static str,
    url: &str,
    key: Option<&str>,
    args: &Args,
    concurrency: usize,
) -> Phase {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .build()
        .unwrap();
    let body = json!({"model": "inference/bench-model",
        "messages": [{"role": "user", "content": "ping"}]})
    .to_string();
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for _ in 0..concurrency {
        let (client, url, body) = (client.clone(), url.to_owned(), body.clone());
        let (stop, measuring) = (stop.clone(), measuring.clone());
        let key = key.map(str::to_owned);
        tasks.push(tokio::spawn(async move {
            let (mut latencies, mut errors) = (Vec::new(), std::collections::BTreeMap::new());
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                let mut request = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(body.clone());
                if let Some(key) = &key {
                    request = request.bearer_auth(key);
                }
                let outcome = match request.send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let _ = response.bytes().await;
                        status
                    }
                    Err(_) => 0,
                };
                if measuring.load(Ordering::Relaxed) {
                    if (200..300).contains(&outcome) {
                        latencies.push(started.elapsed().as_micros() as u64);
                    } else {
                        *errors.entry(outcome).or_insert(0u64) += 1;
                    }
                }
            }
            (latencies, errors)
        }));
    }
    tokio::time::sleep(args.warmup).await;
    measuring.store(true, Ordering::Relaxed);
    let started = Instant::now();
    tokio::time::sleep(args.duration).await;
    let seconds = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let (mut all, mut errors) = (Vec::new(), 0);
    let mut by_status = std::collections::BTreeMap::<u16, u64>::new();
    for task in tasks {
        let (latencies, failed) = task.await.unwrap();
        all.extend(latencies);
        for (status, count) in failed {
            errors += count;
            *by_status.entry(status).or_insert(0) += count;
        }
    }
    all.sort_unstable();
    let at = |q: f64| {
        all.get(((all.len() as f64) * q) as usize)
            .copied()
            .unwrap_or(0)
            .min(all.last().copied().unwrap_or(0))
    };
    Phase {
        name,
        concurrency,
        failures: by_status,
        requests: all.len() as u64,
        errors,
        seconds,
        p50_us: at(0.50),
        p99_us: at(0.99),
        mean_us: if all.is_empty() {
            0
        } else {
            all.iter().sum::<u64>() / all.len() as u64
        },
    }
}

fn rss_kb(pid: u32) -> u64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn capture(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn cpu() -> String {
    let mac = capture("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if mac != "unknown" {
        return mac;
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[tokio::main]
async fn main() {
    if cfg!(debug_assertions) {
        eprintln!("error: run this example with --release (and build the binary with --release)");
        std::process::exit(2);
    }
    let args = parse();
    if !std::path::Path::new(&args.binary).exists() {
        eprintln!(
            "error: {} not found; run `cargo build --release --features serve` first (or pass --bin)",
            args.binary
        );
        std::process::exit(2);
    }
    let upstream = mock(args.think).await;

    let dir = std::env::temp_dir().join(format!("llmshim-serve-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("gateway.toml");
    std::fs::write(
        &config,
        format!(
            "version = 1\n[server]\nport = 0\n[state]\nreceipts_dir = \"{}\"\n\
             [providers.inference]\nkind = \"openai-compatible\"\nbase_url = \"{upstream}\"\n\
             api_key = {{ env = \"BENCH_UPSTREAM_KEY\" }}\n\
             [[keys]]\nname = \"bench\"\ntenant = \"bench\"\nsecret = {{ env = \"BENCH_CALLER_KEY\" }}\n",
            dir.join("receipts").display()
        ),
    )
    .unwrap();
    let mut child = Command::new(&args.binary)
        .args(["serve", "--config", config.to_str().unwrap()])
        .env_clear()
        .env("HOME", &dir)
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env("BENCH_CALLER_KEY", KEY)
        .env("BENCH_UPSTREAM_KEY", UPSTREAM_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cannot start the binary");
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    let base = loop {
        match receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => {
                if let Some(base) = line.split("listening on ").nth(1) {
                    break base.trim().to_owned();
                }
            }
            Err(_) => {
                let _ = child.kill();
                eprintln!("error: the server did not start");
                std::process::exit(1);
            }
        }
    };
    let pid = child.id();
    let idle_kb = rss_kb(pid);

    let sampler_stop = Arc::new(AtomicBool::new(false));
    let peak = {
        let stop = sampler_stop.clone();
        std::thread::spawn(move || {
            let mut peak = 0;
            while !stop.load(Ordering::Relaxed) {
                peak = peak.max(rss_kb(pid));
                std::thread::sleep(Duration::from_millis(250));
            }
            peak
        })
    };
    // Per level: the same load straight at the mock, then through the gateway on
    // the two routes a caller uses: the OpenAI-compatible native wire (which also
    // stores replay metadata for each answer) and llmshim's own `/v1/chat`.
    let mut levels: Vec<(Phase, Phase, Phase)> = Vec::new();
    for (index, &concurrency) in args.levels.iter().enumerate() {
        if index > 0 {
            // A level that failed leaves the machine's ephemeral ports in
            // TIME_WAIT; give them time to drain before the next.
            let dirty = levels
                .last()
                .is_some_and(|l| !l.1.failures.is_empty() || !l.2.failures.is_empty());
            tokio::time::sleep(Duration::from_secs(if dirty { 35 } else { 2 })).await;
        }
        let direct = drive(
            "mock directly",
            &format!("{upstream}/chat/completions"),
            None,
            &args,
            concurrency,
        )
        .await;
        let native = drive(
            "/v1/chat/completions",
            &format!("{base}/v1/chat/completions"),
            Some(KEY),
            &args,
            concurrency,
        )
        .await;
        let own = drive(
            "/v1/chat",
            &format!("{base}/v1/chat"),
            Some(KEY),
            &args,
            concurrency,
        )
        .await;
        levels.push((direct, native, own));
    }
    sampler_stop.store(true, Ordering::Relaxed);
    let peak_kb = peak.join().unwrap().max(rss_kb(pid));
    let after_kb = rss_kb(pid);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    let date = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let sha = capture("git", &["rev-parse", "--short", "HEAD"]);
    let rustc = capture("rustc", &["--version"]);
    let host = capture("uname", &["-srm"]);
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());

    println!("# llmshim serve benchmark\n");
    println!(
        "- date: {date}\n- commit: {sha}\n- build: release, {rustc}\n- host: {host}, {}, {cores} logical cores",
        cpu()
    );
    println!(
        "- load: closed loop at each listed number of concurrent connections, {}s measured after {}s warm-up; \
         mock think time {} ms; client, mock and gateway on one machine\n",
        args.duration.as_secs(),
        args.warmup.as_secs(),
        args.think.as_millis()
    );
    println!(
        "| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    let ms = |us: u64| us as f64 / 1000.0;
    for (direct, native, own) in &levels {
        println!(
            "| {} | mock directly | {} | {} | {:.0} | {:.2} ms | {:.2} ms | | |",
            direct.concurrency,
            direct.requests,
            direct.errors,
            direct.rps(),
            ms(direct.p50_us),
            ms(direct.p99_us)
        );
        for phase in [native, own] {
            println!(
                "| {} | {} | {} | {} | {:.0} | {:.2} ms | {:.2} ms | {:+.2} ms | {:+.2} ms |",
                phase.concurrency,
                phase.name,
                phase.requests,
                phase.errors,
                phase.rps(),
                ms(phase.p50_us),
                ms(phase.p99_us),
                ms(phase.p50_us) - ms(direct.p50_us),
                ms(phase.p99_us) - ms(direct.p99_us)
            );
        }
    }
    println!(
        "\n**Memory (resident set of the serve process, whole run):** {:.1} MiB idle, {:.1} MiB peak, {:.1} MiB after.",
        idle_kb as f64 / 1024.0,
        peak_kb as f64 / 1024.0,
        after_kb as f64 / 1024.0
    );
    for (_, native, own) in &levels {
        for phase in [native, own] {
            if !phase.failures.is_empty() {
                println!(
                    "\nFailures at {} connections on {} (status: count, 0 = transport error): {:?}",
                    phase.concurrency, phase.name, phase.failures
                );
            }
        }
    }
    if let Some(path) = &args.json {
        let row = |p: &Phase| {
            json!({"path": p.name, "connections": p.concurrency, "requests": p.requests,
                "failed": p.errors, "failures": p.failures, "rps": p.rps(),
                "p50_us": p.p50_us, "p99_us": p.p99_us, "mean_us": p.mean_us})
        };
        let report = json!({"date": date, "commit": sha, "rustc": rustc, "host": host, "cpu": cpu(),
            "cores": cores, "duration_secs": args.duration.as_secs(),
            "warmup_secs": args.warmup.as_secs(), "think_ms": args.think.as_millis() as u64,
            "levels": levels.iter().map(|(d, n, o)| json!({"direct": row(d), "native": row(n), "own": row(o)})).collect::<Vec<_>>(),
            "rss_kib": {"idle": idle_kb, "peak": peak_kb, "after": after_kb}});
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    // The first level is the sanity check: if even the lightest load fails, the
    // run is not a result. Failures at higher levels are findings, reported above.
    let (direct, native, own) = &levels[0];
    if [direct, native, own]
        .iter()
        .any(|phase| phase.errors > 0 || phase.requests == 0)
    {
        eprintln!(
            "error: requests failed at the lightest level; the figures above are not a result"
        );
        std::process::exit(1);
    }
}
