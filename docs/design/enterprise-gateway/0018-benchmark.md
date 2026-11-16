# 0018: Benchmark

Status: implemented. `benchmarks/serve_bench.rs` (the `serve_bench` example, built
with `--features serve`) and the first published run. Bar item 6 of 0013 asks for a
benchmark published with its method, hardware, build and date, reporting added
latency (p50 and p99), throughput and memory against a mock upstream.

## Method

A local mock answers `POST /chat/completions` at once (no think time unless
`--think-ms` is given). At each concurrency level the example drives the same
closed-loop load three ways, for 15 s after a 3 s warm-up, with one client per
connection and keep-alive: straight at the mock; through a real `llmshim serve`
process on `/v1/chat/completions` (the OpenAI-compatible native wire, which also
stores replay metadata for each answer); and through it on `/v1/chat` (llmshim's own
route). **Added latency** is the gateway phase's p50/p99 minus the direct phase's at
the same level; throughput is requests answered 2xx per second; memory is the
serve process's resident set read with `ps` at start, sampled every 250 ms, and at
the end. A request that does not answer 2xx is counted as failed and is not in the
latency figures.

The client, the mock and the gateway share one machine, so absolute figures
include that contention (the direct phase reaches 100,000 requests per second,
which is the ceiling of the harness, not of anything under test). The release build
is required; a debug build is refused. Costs nothing and contacts no provider.

```text
cargo build --release --features serve
cargo run --release --features serve --example serve_bench -- \
    --concurrency 1,8,32,64 --json out.json
```

The run exits non-zero only if the **lightest** level has a failure; failures at
higher levels are findings, listed under the table, because they are what the run
exists to find.

## Result (2026-11-16, commit cbf394b)

- date: 2026-11-16T07:37:33Z
- commit: cbf394b
- build: release, rustc 1.97.0 (2d8144b78 2026-07-07)
- host: Darwin 27.2.0 arm64, Apple M5 Pro, 18 logical cores
- load: closed loop at each listed number of concurrent connections, 15s measured after 3s warm-up; mock think time 0 ms; client, mock and gateway on one machine

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 338936 | 0 | 22594 | 0.04 ms | 0.12 ms | | |
| 1 | /v1/chat/completions | 77640 | 0 | 5176 | 0.18 ms | 0.52 ms | +0.14 ms | +0.40 ms |
| 1 | /v1/chat | 95471 | 0 | 6364 | 0.15 ms | 0.26 ms | +0.11 ms | +0.14 ms |
| 8 | mock directly | 1412139 | 0 | 94136 | 0.08 ms | 0.15 ms | | |
| 8 | /v1/chat/completions | 330077 | 7 | 22004 | 0.36 ms | 0.51 ms | +0.28 ms | +0.36 ms |
| 8 | /v1/chat | 111191 | 13 | 7412 | 0.23 ms | 0.47 ms | +0.15 ms | +0.32 ms |
| 32 | mock directly | 1682025 | 0 | 112127 | 0.28 ms | 0.45 ms | | |
| 32 | /v1/chat/completions | 12560 | 120 | 837 | 0.29 ms | 0.65 ms | +0.01 ms | +0.21 ms |
| 32 | /v1/chat | 58952 | 111 | 3929 | 0.86 ms | 2.04 ms | +0.58 ms | +1.59 ms |
| 64 | mock directly | 1757086 | 0 | 117132 | 0.54 ms | 0.88 ms | | |
| 64 | /v1/chat/completions | 3799 | 319 | 253 | 0.32 ms | 1.08 ms | -0.22 ms | +0.20 ms |
| 64 | /v1/chat | 36815 | 270 | 2454 | 3.10 ms | 4.75 ms | +2.56 ms | +3.87 ms |

**Memory (resident set of the serve process, whole run):** 10.2 MiB idle, 101.4 MiB peak, 101.4 MiB after.

Failures at 8 connections on /v1/chat/completions (status: count, 0 = transport error): {503: 7}

Failures at 8 connections on /v1/chat (status: count, 0 = transport error): {502: 13}

Failures at 32 connections on /v1/chat/completions (status: count, 0 = transport error): {502: 120}

Failures at 32 connections on /v1/chat (status: count, 0 = transport error): {502: 111}

Failures at 64 connections on /v1/chat/completions (status: count, 0 = transport error): {502: 319}

Failures at 64 connections on /v1/chat (status: count, 0 = transport error): {502: 270}


## Reading it

- **At one connection** the gateway adds **about 0.1 ms at p50** (0.14 ms on the
  native wire, 0.11 ms on `/v1/chat`) and **0.14 to 0.40 ms at p99**, with no
  failures, and serves 5,000 to 6,400 requests per second from one client.
- **Memory** is about 10 MiB idle and about 100 MiB at the peak of the run.
- **From 8 connections up the figures are not the gateway's cost.** The failures
  and the collapsing throughput come from the two limits below, and a latency
  computed from the few answers that got through (the negative "added" figures at
  64 connections) means nothing. Do not quote those rows as overhead.

## Findings

The benchmark found two limits; neither is changed here, because each is shared
code with its own decision.

1. **The upstream connection pool keeps 4 idle connections per host**
   (`src/client.rs`, `pool_max_idle_per_host(4)`, one shared client for every
   build). With more than a few requests in flight at once, every extra request
   opens and then closes its own connection. On the benchmark host that put the
   ephemeral port range into `TIME_WAIT` within seconds (16,288 sockets counted
   during a 64-connection run, the whole range) and requests failed with 502
   `error sending request`. Against a real provider the same churn is a TLS
   handshake per request. This is the last row of 0002 (the process-wide client
   settings need a per-router client); it is now also the reason that row matters.
2. **The replay-receipt executor admits twelve operations at a time**
   (`src/proxy/wire/mod.rs`: two workers, 8 ingress and 4 egress slots), and a
   request beyond that is answered 503 `native replay metadata is busy`. The native
   wire stores replay metadata for every answer, so at roughly a dozen requests in
   flight on `/v1/chat/completions` a few are refused (7 of 330,000 at 8
   connections in the run above). Whether an answer with no reasoning block or tool
   call needs a receipt at all is the question to settle.

Both are on the roadmap. Until they are settled, a deployment should not be sized
from a throughput measured above one connection.

## Not measured

Think time (a real provider's latency hides added latency), streaming, a fleet
(Redis in the path), TLS termination, and a longer soak. `--think-ms` exists for
the first; the others are future runs of the same harness.
