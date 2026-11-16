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
    /// Failed requests by status (0 = transport error), with how long they took.
    failures: std::collections::BTreeMap<u16, (u64, u64)>,
    /// The first failing body seen, cut short, so the cause is on the record.
    sample: Option<String>,
    requests: u64,
    errors: u64,
    seconds: f64,
    p50_us: u64,
    p99_us: u64,
    mean_us: u64,
    /// The serve process's peak resident set during this phase, when sampled.
    peak_kb: Option<u64>,
}

impl Phase {
    fn rps(&self) -> f64 {
        self.requests as f64 / self.seconds
    }

    /// Mean time a failed request took, in ms.
    fn failed_mean_ms(&self) -> f64 {
        let (count, micros) = self
            .failures
            .values()
            .fold((0, 0), |(c, m), (n, us)| (c + n, m + us));
        if count == 0 {
            0.0
        } else {
            micros as f64 / count as f64 / 1000.0
        }
    }
}

#[derive(Default)]
struct Outcomes {
    latencies: Vec<u64>,
    failures: std::collections::BTreeMap<u16, (u64, u64)>,
    sample: Option<String>,
}

async fn drive(
    name: &'static str,
    url: &str,
    key: Option<&str>,
    args: &Args,
    concurrency: usize,
    serve_pid: Option<u32>,
) -> Phase {
    // The harness's own client never takes an ambient proxy: loopback traffic
    // must not leave the machine.
    let client = reqwest::Client::builder()
        .no_proxy()
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
            let mut out = Outcomes::default();
            while !stop.load(Ordering::Relaxed) {
                // Counted only if it began and ended inside the measured window.
                let began_inside = measuring.load(Ordering::Relaxed);
                let started = Instant::now();
                let mut request = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(body.clone());
                if let Some(key) = &key {
                    request = request.bearer_auth(key);
                }
                let (status, text) = match request.send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        (status, response.text().await.unwrap_or_default())
                    }
                    Err(error) => (0, format!("{error}")),
                };
                let elapsed = started.elapsed().as_micros() as u64;
                if began_inside && measuring.load(Ordering::Relaxed) {
                    if (200..300).contains(&status) {
                        out.latencies.push(elapsed);
                    } else {
                        let entry = out.failures.entry(status).or_insert((0, 0));
                        entry.0 += 1;
                        entry.1 += elapsed;
                        if out.sample.is_none() {
                            out.sample = Some(format!(
                                "{status}: {}",
                                text.chars().take(160).collect::<String>()
                            ));
                        }
                    }
                }
            }
            out
        }));
    }
    tokio::time::sleep(args.warmup).await;
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler = serve_pid.map(|pid| {
        let stop = sampler_stop.clone();
        std::thread::spawn(move || {
            let mut peak = 0;
            while !stop.load(Ordering::Relaxed) {
                peak = peak.max(rss_kb(pid));
                std::thread::sleep(Duration::from_millis(250));
            }
            peak.max(rss_kb(pid))
        })
    });
    measuring.store(true, Ordering::Relaxed);
    let started = Instant::now();
    tokio::time::sleep(args.duration).await;
    measuring.store(false, Ordering::Relaxed);
    let seconds = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    sampler_stop.store(true, Ordering::Relaxed);
    let (mut all, mut errors) = (Vec::new(), 0);
    let mut by_status = std::collections::BTreeMap::<u16, (u64, u64)>::new();
    let mut sample = None;
    for task in tasks {
        let out = task.await.expect("a load task panicked");
        all.extend(out.latencies);
        sample = sample.or(out.sample);
        for (status, (count, micros)) in out.failures {
            errors += count;
            let entry = by_status.entry(status).or_insert((0, 0));
            entry.0 += count;
            entry.1 += micros;
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
        sample,
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
        peak_kb: sampler.map(|handle| handle.join().unwrap_or(0)),
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
    let mut args = parse();
    args.levels.sort_unstable();
    args.levels.dedup();
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
    let child = Command::new(&args.binary)
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
    // From here the guard owns the process and the directory: however the run
    // ends (a panic, a failed start), the server is stopped and the files go.
    let mut server = Server { child, dir };
    let stderr = server.child.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    let mut seen = Vec::new();
    let base = loop {
        match receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => {
                if let Some(base) = line.split("listening on ").nth(1) {
                    break base.trim().to_owned();
                }
                seen.push(line);
            }
            Err(_) => {
                eprintln!("error: the server did not start; it said: {seen:#?}");
                std::process::exit(1);
            }
        }
    };
    let pid = server.child.id();
    let idle_kb = rss_kb(pid);
    let open_files = capture("sh", &["-c", "ulimit -n"]);
    let settle = |dirty: bool| tokio::time::sleep(Duration::from_secs(if dirty { 35 } else { 2 }));

    // Per level, ascending (so the first is the lightest): the same load straight
    // at the mock, then through the gateway on the two routes a caller uses: the
    // OpenAI-compatible native wire (which also stores replay metadata for each
    // answer) and llmshim's own `/v1/chat`. A phase that failed leaves the
    // machine's ephemeral ports in TIME_WAIT, so the next phase waits for them.
    let mut levels: Vec<(Phase, Phase, Phase)> = Vec::new();
    let mut time_wait: Vec<(usize, &'static str, String)> = Vec::new();
    let mut dirty = false;
    for &concurrency in &args.levels {
        if !levels.is_empty() {
            settle(dirty).await;
        }
        let direct = drive(
            "mock directly",
            &format!("{upstream}/chat/completions"),
            None,
            &args,
            concurrency,
            None,
        )
        .await;
        settle(false).await;
        let native = drive(
            "/v1/chat/completions",
            &format!("{base}/v1/chat/completions"),
            Some(KEY),
            &args,
            concurrency,
            Some(pid),
        )
        .await;
        time_wait.push((concurrency, "/v1/chat/completions", sockets_in_time_wait()));
        settle(!native.failures.is_empty()).await;
        let own = drive(
            "/v1/chat",
            &format!("{base}/v1/chat"),
            Some(KEY),
            &args,
            concurrency,
            Some(pid),
        )
        .await;
        time_wait.push((concurrency, "/v1/chat", sockets_in_time_wait()));
        dirty = !own.failures.is_empty() || !native.failures.is_empty();
        levels.push((direct, native, own));
    }
    let after_kb = rss_kb(pid);
    drop(server);

    let date = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let dirty_tree = capture("git", &["status", "--porcelain"]) != "unknown";
    let sha = format!(
        "{}{}",
        capture("git", &["rev-parse", "--short", "HEAD"]),
        if dirty_tree {
            " (working tree not clean)"
        } else {
            ""
        }
    );
    let rustc = capture("rustc", &["--version"]);
    let host = capture("uname", &["-srm"]);
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());

    println!("# llmshim serve benchmark\n");
    println!(
        "- date: {date}\n- commit: {sha}\n- build: release, {rustc}\n- host: {host}, {}, {cores} logical cores, open-file limit {open_files}",
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
        "| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS |\n\
         | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    let ms = |us: u64| us as f64 / 1000.0;
    for (direct, native, own) in &levels {
        println!(
            "| {} | mock directly | {} | {} | {:.0} | {:.2} ms | {:.2} ms | | | |",
            direct.concurrency,
            direct.requests,
            direct.errors,
            direct.rps(),
            ms(direct.p50_us),
            ms(direct.p99_us)
        );
        for phase in [native, own] {
            println!(
                "| {} | {} | {} | {} | {:.0} | {:.2} ms | {:.2} ms | {:+.2} ms | {:+.2} ms | {:.1} MiB |",
                phase.concurrency,
                phase.name,
                phase.requests,
                phase.errors,
                phase.rps(),
                ms(phase.p50_us),
                ms(phase.p99_us),
                ms(phase.p50_us) - ms(direct.p50_us),
                ms(phase.p99_us) - ms(direct.p99_us),
                phase.peak_kb.unwrap_or(0) as f64 / 1024.0
            );
        }
    }
    println!(
        "\n**Memory (resident set of the serve process):** {:.1} MiB idle before any load, {:.1} MiB after the last phase.",
        idle_kb as f64 / 1024.0,
        after_kb as f64 / 1024.0
    );
    for (_, native, own) in &levels {
        for phase in [native, own] {
            if !phase.failures.is_empty() {
                let by_status: Vec<String> = phase
                    .failures
                    .iter()
                    .map(|(status, (count, _))| format!("{status}: {count}"))
                    .collect();
                println!(
                    "\nFailures at {} connections on {}: {} (status: count, 0 = transport error); \
                     a failed request took {:.1} ms on average; first body: {}",
                    phase.concurrency,
                    phase.name,
                    by_status.join(", "),
                    phase.failed_mean_ms(),
                    phase.sample.as_deref().unwrap_or("-")
                );
            }
        }
    }
    println!("\nSockets in TIME_WAIT on the host at the end of each gateway phase:");
    for (concurrency, path, count) in &time_wait {
        println!("- {concurrency} connections, {path}: {count}");
    }
    if let Some(path) = &args.json {
        let row = |p: &Phase| {
            json!({"path": p.name, "connections": p.concurrency, "requests": p.requests,
                "failed": p.errors,
                "failures": p.failures.iter().map(|(s, (n, us))| (s.to_string(), json!({"count": n, "total_us": us}))).collect::<serde_json::Map<_, _>>(),
                "first_failure": p.sample, "rps": p.rps(),
                "p50_us": p.p50_us, "p99_us": p.p99_us, "mean_us": p.mean_us,
                "peak_rss_kib": p.peak_kb})
        };
        let report = json!({"date": date, "commit": sha, "rustc": rustc, "host": host, "cpu": cpu(),
            "cores": cores, "open_file_limit": open_files, "duration_secs": args.duration.as_secs(),
            "warmup_secs": args.warmup.as_secs(), "think_ms": args.think.as_millis() as u64,
            "levels": levels.iter().map(|(d, n, o)| json!({"direct": row(d), "native": row(n), "own": row(o)})).collect::<Vec<_>>(),
            "time_wait": time_wait.iter().map(|(c, p, n)| json!({"connections": c, "path": p, "sockets": n})).collect::<Vec<_>>(),
            "rss_kib": {"idle": idle_kb, "after": after_kb}});
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    // The first level is the lightest (the list is sorted): if even that fails,
    // the run is not a result. Failures at higher levels are findings, reported
    // above.
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

/// The server process and its scratch directory, removed however the run ends.
struct Server {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sockets_in_time_wait() -> String {
    capture("sh", &["-c", "netstat -an | grep -c TIME_WAIT"])
}
