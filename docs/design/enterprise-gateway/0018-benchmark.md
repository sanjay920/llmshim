# 0018: Benchmark

Status: implemented. `benchmarks/serve_bench.rs` (the `serve_bench` example, built
with `--features serve`) and the first published run. Bar item 6 of 0013 asks for a
benchmark published with its method, hardware, build and date, reporting added
latency (p50 and p99), throughput and memory against a mock upstream.

## Method

A local mock answers `POST /chat/completions` at once (no think time unless
`--think-ms` is given). At each concurrency level, ascending, the example drives the
same closed-loop load three ways, for 15 s after a 3 s warm-up: straight at the
mock; through a real `llmshim serve` process on `/v1/chat/completions` (the
OpenAI-compatible native wire, which also stores replay metadata for each answer);
and through it on `/v1/chat` (llmshim's own route). Each phase has one HTTP client
with a connection pool as large as the level and that many tasks, each sending its
next request when the last one answers. **Added latency** is the gateway phase's
p50/p99 minus the direct phase's at the same level. Throughput is requests answered
2xx per second, counting only requests that began and ended inside the measured
window. A request that does not answer 2xx is counted as failed and left out of the
latency figures; the report gives how long failures took and the first failing body,
because without them the latency columns hide the cost of failure. Memory is the
serve process's resident set read with `ps`: once before any load, every 250 ms
during each gateway phase (the phase's peak), and after the last phase. A phase
with failures is followed by 35 s for the host's ephemeral ports to drain.

The client, the mock and the gateway share one machine, so absolute figures
include that contention (the direct phase reaches 100,000 requests per second,
which is the ceiling of the harness, not of anything under test). The release build
is required; a debug build is refused. The harness starts and stops the server
itself, and the binary it runs is `target/release/llmshim` (`--bin` to change it):
build it first, from the commit you mean to measure, because `cargo run --example`
does not rebuild it. The report names the commit and says when the tree is not
clean. Costs nothing and contacts no provider.

```text
cargo build --release --features serve
cargo run --release --features serve --example serve_bench -- \
    --concurrency 1,8,32,64 --json out.json
```

The run exits non-zero only if the **lightest** level has a failure; failures at
higher levels are findings, listed under the table, because they are what the run
exists to find.

## Result (2026-11-16, commit 74f434d)

- date: 2026-11-16T08:07:10Z
- commit: 74f434d
- build: release, rustc 1.97.0 (2d8144b78 2026-07-07)
- host: Darwin 27.2.0 arm64, Apple M5 Pro, 18 logical cores, open-file limit 1048576
- load: closed loop at each listed number of concurrent connections, 15s measured after 3s warm-up; mock think time 0 ms; client, mock and gateway on one machine

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 335574 | 0 | 22370 | 0.04 ms | 0.10 ms | | | |
| 1 | /v1/chat/completions | 68523 | 0 | 4568 | 0.19 ms | 0.60 ms | +0.15 ms | +0.50 ms | 91.2 MiB |
| 1 | /v1/chat | 91941 | 0 | 6129 | 0.16 ms | 0.30 ms | +0.11 ms | +0.20 ms | 91.3 MiB |
| 8 | mock directly | 1386809 | 0 | 92448 | 0.08 ms | 0.15 ms | | | |
| 8 | /v1/chat/completions | 298776 | 3 | 19917 | 0.39 ms | 0.59 ms | +0.31 ms | +0.43 ms | 92.6 MiB |
| 8 | /v1/chat | 157504 | 11 | 10499 | 0.36 ms | 0.57 ms | +0.28 ms | +0.42 ms | 92.8 MiB |
| 32 | mock directly | 1606784 | 0 | 107112 | 0.29 ms | 0.48 ms | | | |
| 32 | /v1/chat/completions | 4311 | 95 | 287 | 0.32 ms | 0.58 ms | +0.03 ms | +0.10 ms | 72.2 MiB |
| 32 | /v1/chat | 6990 | 93 | 466 | 0.21 ms | 0.58 ms | -0.08 ms | +0.09 ms | 72.5 MiB |
| 64 | mock directly | 1651577 | 0 | 110098 | 0.57 ms | 0.95 ms | | | |
| 64 | /v1/chat/completions | 2001 | 206 | 133 | 0.34 ms | 1.41 ms | -0.22 ms | +0.46 ms | 76.7 MiB |
| 64 | /v1/chat | 72 | 214 | 5 | 0.49 ms | 5327.95 ms | -0.08 ms | +5327.01 ms | 76.6 MiB |

**Memory (resident set of the serve process):** 10.2 MiB idle before any load, 39.3 MiB after the last phase.

Failures at 8 connections on /v1/chat/completions: 503: 3 (status: count, 0 = transport error); a failed request took 0.3 ms on average; first body: 503: {"error":{"code":null,"message":"native replay metadata is busy; retry the request","param":null,"type":"api_error"}}

Failures at 8 connections on /v1/chat: 502: 11 (status: count, 0 = transport error); a failed request took 3661.3 ms on average; first body: 502: {"error":{"code":"provider_error","message":"HTTP error: error sending request"}}

Failures at 32 connections on /v1/chat/completions: 502: 95 (status: count, 0 = transport error); a failed request took 3397.9 ms on average; first body: 502: {"error":{"code":null,"message":"HTTP error: error sending request","param":null,"type":"api_error"}}

Failures at 32 connections on /v1/chat: 502: 93 (status: count, 0 = transport error); a failed request took 3374.7 ms on average; first body: 502: {"error":{"code":"provider_error","message":"HTTP error: error sending request"}}

Failures at 64 connections on /v1/chat/completions: 502: 206 (status: count, 0 = transport error); a failed request took 3341.1 ms on average; first body: 502: {"error":{"code":null,"message":"HTTP error: error sending request","param":null,"type":"api_error"}}

Failures at 64 connections on /v1/chat: 502: 214 (status: count, 0 = transport error); a failed request took 3113.6 ms on average; first body: 502: {"error":{"code":"provider_error","message":"HTTP error: error sending request"}}

(The commit is on the branch that added the benchmark, merged to `main` with its
history.)

## Reading it

- **At one connection** the gateway adds **about 0.1 ms at p50** (0.15 ms on the
  native wire, 0.11 ms on `/v1/chat`) and **0.2 to 0.5 ms at p99**, with no
  failures, and serves 4,600 to 6,100 requests per second from one client.
- **Memory.** About 10 MiB idle. After the first requests the resident set is
  about 91 MiB and stays between roughly 70 and 95 MiB through the run (the rows
  above; macOS compresses pages, so a figure can fall between phases). What the
  first request allocates was not investigated; the catalog, whose first read folds
  its vendored data (CLAUDE.md), is the first suspect.
- **From 8 connections up the figures are not the gateway's cost.** Failures and
  collapsing throughput come from the limits below, and a latency computed from the
  few answers that got through (the negative "added" figures) means nothing. Do not
  quote those rows as overhead.

## Findings

The benchmark found two limits; neither is changed here, because each is shared
code with its own decision.

1. **Connections to the upstream fail once more than a few requests are in flight,
   and the shared client keeps only 4 idle connections per host.** The facts:
   - Every failure is a 502 `HTTP error: error sending request` (a transport error
     from the gateway's client to the mock); the mock answered the harness directly
     at 110,000 requests per second with none.
   - A failed request takes about 3.1 to 3.7 s on average, which is the gateway's
     reactive retry with backoff (`src/client.rs`). In a closed loop that is what
     collapses throughput: 95 failures of about 3.4 s each is more than the whole
     measured window of the 32 connections that carried them.
   - The shared client is built with `pool_max_idle_per_host(4)` (`src/client.rs`,
     one client for every build).
   - The host's open-file limit was 1,048,576, so the descriptor limit is not the
     cause.
   - **Observed by hand, not by the harness:** during a 64-connection run,
     `netstat -an -p tcp | grep -c TIME_WAIT` rose from about 18 to 16,286 within
     three seconds of the gateway phase starting and stayed there for 30 s (the
     ephemeral range on this host is 49152 to 65535, so 16,384 ports). That is what
     opening and closing a connection per request beyond the 4 idle ones would
     look like. The harness's own probe could not be kept: the same command read
     nothing when spawned from the harness process on this host, so no automated
     figure is published.

   The explanation is the best one the evidence supports; it was not isolated by an
   experiment. The experiment (a larger pool, same run) belongs with the fix. This
   is the last row of 0002 (the process-wide client settings need a per-router
   client), and now also the reason that row matters: against a real provider the
   same churn is a TLS handshake per request.
2. **The replay-receipt executor refuses work immediately when its slots are
   taken** (`src/proxy/wire/mod.rs`: two workers, 8 ingress slots and 4 egress
   slots, each class taken without waiting; a request beyond them is answered 503
   `native replay metadata is busy`). The native wire stores replay metadata for
   every answer, so a few requests are refused even at 8 connections (3 of
   298,779 in the run above, each in 0.3 ms). Whether an answer with no reasoning
   block or tool call needs a receipt at all is the question to settle.

Both are on the roadmap. Until they are settled, a deployment should not be sized
from a throughput measured above one connection.

## Not measured

Think time (a real provider's latency hides added latency), streaming, a fleet
(Redis in the path), TLS termination, and a longer soak. `--think-ms` exists for
the first; the others are future runs of the same harness.
