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

The benchmark found two limits; neither was changed in the commit that published
this run (the pool was changed afterwards, by 0021: see the second run below), because each is shared
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

*(Superseded in part by the second run below: the pool is resolved for `serve`; the
receipt executor is not.)* Both are on the roadmap. Until they are settled, a deployment should not be sized
from a throughput measured above one connection.

## Not measured

Think time (a real provider's latency hides added latency), streaming, a fleet
(Redis in the path), TLS termination, and a longer soak. `--think-ms` exists for
the first; the others are future runs of the same harness.

## Second run (2026-11-16, commit 4327310): after 0021

The change 0021 made (the router's own client, with a pool that follows the
concurrency served: 256 here) was measured with the same harness, on the same
machine, with one addition: the harness now counts the connections the mock
accepted in each phase, so the cause no longer rests on a hand-read `TIME_WAIT`
figure. The same command; the only differences from the first run are the code
and that extra column.

- date: 2026-11-16T16:48:07Z
- commit: 4327310
- build: release, rustc 1.97.0 (2d8144b78 2026-07-07)
- host: Darwin 27.2.0 arm64, Apple M5 Pro, 18 logical cores, open-file limit 1048576
- load: closed loop at each listed number of concurrent connections, 15s measured after 3s warm-up; mock think time 0 ms; client, mock and gateway on one machine

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS | upstream connections |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 319409 | 0 | 21292 | 0.05 ms | 0.06 ms | | | | 1 |
| 1 | /v1/chat/completions | 76613 | 0 | 5107 | 0.19 ms | 0.23 ms | +0.15 ms | +0.17 ms | 93.3 MiB | 1 |
| 1 | /v1/chat | 98232 | 0 | 6548 | 0.15 ms | 0.18 ms | +0.10 ms | +0.12 ms | 93.4 MiB | 0 |
| 8 | mock directly | 1395183 | 0 | 93006 | 0.08 ms | 0.14 ms | | | | 8 |
| 8 | /v1/chat/completions | 307918 | 67 | 20526 | 0.39 ms | 0.53 ms | +0.30 ms | +0.39 ms | 97.1 MiB | 7 |
| 8 | /v1/chat | 378115 | 0 | 25206 | 0.31 ms | 0.45 ms | +0.23 ms | +0.31 ms | 94.9 MiB | 0 |
| 32 | mock directly | 1803541 | 0 | 120228 | 0.26 ms | 0.41 ms | | | | 32 |
| 32 | /v1/chat/completions | 485508 | 84800 | 32365 | 0.89 ms | 1.23 ms | +0.63 ms | +0.82 ms | 101.4 MiB | 13 |
| 32 | /v1/chat | 555488 | 0 | 37030 | 0.84 ms | 1.57 ms | +0.58 ms | +1.15 ms | 101.9 MiB | 11 |
| 64 | mock directly | 1839764 | 0 | 122643 | 0.52 ms | 0.81 ms | | | | 65 |
| 64 | /v1/chat/completions | 451686 | 167729 | 30110 | 1.71 ms | 2.19 ms | +1.20 ms | +1.37 ms | 105.1 MiB | 24 |
| 64 | /v1/chat | 585981 | 0 | 39063 | 1.63 ms | 2.09 ms | +1.11 ms | +1.28 ms | 106.1 MiB | 5 |

`upstream connections` is how many connections the mock accepted during the phase (warm-up included): what was opened to the upstream, by the client directly in the first row of a level and by the gateway in the others.

**Memory (resident set of the serve process):** 11.0 MiB idle before any load, 106.1 MiB after the last phase.

Failures at 8 connections on /v1/chat/completions: 503: 67 (status: count, 0 = transport error); a failed request took 0.4 ms on average; first body: 503: {"error":{"code":null,"message":"native replay metadata is busy; retry the request","param":null,"type":"api_error"}}

Failures at 32 connections on /v1/chat/completions: 503: 84800 (status: count, 0 = transport error); a failed request took 0.5 ms on average; first body: 503: {"error":{"code":null,"message":"native replay metadata is busy; retry the request","param":null,"type":"api_error"}}

Failures at 64 connections on /v1/chat/completions: 503: 167729 (status: count, 0 = transport error); a failed request took 1.1 ms on average; first body: 503: {"error":{"code":null,"message":"native replay metadata is busy; retry the request","param":null,"type":"api_error"}}

### What changed, and what did not

- **The connection churn is gone.** Every 502 `error sending request` of the first
  run is absent, at every level. On `/v1/chat` there are **no failures at 32 and 64
  connections**, throughput there is **37,000 and 39,000 requests per second** (the
  first run: 466 and 5), and the gateway opened **11 and 5 connections to the
  upstream in the whole phase** (warm-up included), where a connection per request
  would have been hundreds of thousands. (The gateway's upstream pool persists
  across phases and levels, so these are the connections opened *in* each phase, on
  top of idle ones inherited from the one before: the `/v1/chat` phase at 64
  inherits what the native phase just opened, which is why it shows 5, and the
  gateway never held 64 upstream connections at once here. The conclusion is that
  it no longer fails at tens in flight, not that 5 is a cold-start cost.) Added latency is **+0.58 ms at p50 and
  +1.15 ms at p99 at 32 connections**, and +1.11 / +1.28 ms at 64, with every
  request answered. This comparison changes the code and the pool default
  together, so it is consistent with the pool explanation but does not isolate it;
  the isolating run below does.
- **The other finding is now the only one left, and larger.** The native wire
  (`/v1/chat/completions`) still answers 503 `native replay metadata is busy`: 67 of
  308,000 at 8 connections, **84,800 of 570,000 (15 %) at 32 and 167,000 of 619,000
  (27 %) at 64**, each refused in about half a millisecond. That is the receipt
  executor's 8 ingress and 4 egress slots (finding 2); it was hidden before by the
  connection failures. Throughput on the native wire counts only the requests that
  were answered 2xx; **the latency and "added" columns of those rows describe only
  those survivors (85 % at 32, 73 % at 64) and are not the gateway's cost on that
  route; do not quote them**.
- **A caveat on the comparison.** Both runs are one machine, with the client, the
  mock and the gateway sharing it; the figures are for comparing the two runs, not
  absolute capacity. The mock is HTTP/1.1; an HTTP/2 provider would not have shown
  the first run's failure at all.
- **Memory** is about 11 MiB idle and 93 to 106 MiB under load, as before.

### The isolating run: the same binary, the pool set back to 4

The second run changes the code and the pool default together. To isolate the
pool, the harness gained `--pool N`, which writes `[upstream]
pool_max_idle_per_host = N` into the gateway's file; this run is the **same
release binary** as the second run (the example alone was rebuilt), with only that
one setting changed to the old value of 4 (commit 3d9646b, 2026-11-16):

- date: 2026-11-16T16:52:43Z
- commit: 3d9646b
- build: release, rustc 1.97.0 (2d8144b78 2026-07-07)
- host: Darwin 27.2.0 arm64, Apple M5 Pro, 18 logical cores, open-file limit 1048576
- load: closed loop at each listed number of concurrent connections, 15s measured after 3s warm-up; mock think time 0 ms; client, mock and gateway on one machine
- gateway `[upstream] pool_max_idle_per_host`: 4

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS | upstream connections |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 321832 | 0 | 21454 | 0.05 ms | 0.06 ms | | | | 1 |
| 1 | /v1/chat/completions | 76815 | 0 | 5121 | 0.19 ms | 0.23 ms | +0.15 ms | +0.17 ms | 93.3 MiB | 1 |
| 1 | /v1/chat | 100838 | 0 | 6722 | 0.15 ms | 0.18 ms | +0.10 ms | +0.12 ms | 93.4 MiB | 0 |
| 32 | mock directly | 1695145 | 0 | 113001 | 0.28 ms | 0.44 ms | | | | 32 |
| 32 | /v1/chat/completions | 101 | 102 | 7 | 0.41 ms | 4276.71 ms | +0.13 ms | +4276.27 ms | 98.4 MiB | 16283 |
| 32 | /v1/chat | 45 | 104 | 3 | 0.42 ms | 3930.01 ms | +0.14 ms | +3929.57 ms | 98.7 MiB | 16316 |

`upstream connections` is how many connections the mock accepted during the phase (warm-up included): what was opened to the upstream, by the client directly in the first row of a level and by the gateway in the others.

**Memory (resident set of the serve process):** 11.0 MiB idle before any load, 98.7 MiB after the last phase.

Failures at 32 connections on /v1/chat/completions: 502: 102 (status: count, 0 = transport error); a failed request took 3160.6 ms on average; first body: 502: {"error":{"code":null,"message":"HTTP error: error sending request","param":null,"type":"api_error"}}

Failures at 32 connections on /v1/chat: 502: 104 (status: count, 0 = transport error); a failed request took 3292.1 ms on average; first body: 502: {"error":{"code":"provider_error","message":"HTTP error: error sending request"}}

With the pool at 4 the failure comes back exactly as in the first run, on the same
binary that served 37,000 requests per second a few minutes earlier: at 32
concurrent requests `/v1/chat` falls to **3 requests per second** (the native wire to
7), every failure is the 502 `error sending request`, and a failed request takes
about 3 s (the retry backoff). The new column shows the mechanism directly: the mock
accepted **16,283 and 16,316 connections** in a phase, where the default pool opened
13 and 11. That is a connection per request until the machine's ephemeral port range
(16,384 ports on this host) is spent, which is what the hand-read `TIME_WAIT` count
in the first run suggested, now measured by the harness and not by hand.

### Status of the findings

1. *Connections to the upstream fail once more than a few requests are in flight*
   (pool of 4 idle per host): **resolved for `serve`** by 0021, and **isolated**:
   the same binary fails with a pool of 4 and does not with the default (above).
   The library's own default stays 4.
2. *The replay-receipt executor refuses work when its slots are taken*: **open**,
   and now the dominant limit on the native wire above a dozen requests in flight.
   Whether an answer with no reasoning block or tool call needs a receipt at all
   is still the question to settle.

## Third run (2026-11-16, commit cf8b7e8): after 0023

Binary built from cf8b7e8; the harness is that commit plus the `--history` flag added in
the commit that records this run (the report's "working tree not clean" is that flag).
Same host (Darwin 27.2.0 arm64, Apple M5 Pro, 18 logical cores), same method, release
build, 15 s measured after 3 s warm-up, mock think time 0 ms. The harness gained
`--history N` (earlier user/assistant text turns carried in every request), so the case
the first two runs could not show is measured too. The first table is the request the
earlier runs sent (a single user message); the second carries two earlier turns.

**One user message** (`--concurrency 1,8,32,64`):

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS | upstream connections |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 330324 | 0 | 22020 | 0.04 ms | 0.06 ms | | | | 1 |
| 1 | /v1/chat/completions | 92786 | 0 | 6185 | 0.16 ms | 0.20 ms | +0.11 ms | +0.14 ms | 90.8 MiB | 1 |
| 1 | /v1/chat | 99864 | 0 | 6657 | 0.15 ms | 0.19 ms | +0.10 ms | +0.13 ms | 90.8 MiB | 0 |
| 8 | mock directly | 1356092 | 0 | 90400 | 0.09 ms | 0.15 ms | | | | 8 |
| 8 | /v1/chat/completions | 347704 | 0 | 23179 | 0.34 ms | 0.48 ms | +0.25 ms | +0.34 ms | 92.2 MiB | 7 |
| 8 | /v1/chat | 356420 | 0 | 23760 | 0.33 ms | 0.47 ms | +0.25 ms | +0.33 ms | 92.2 MiB | 0 |
| 32 | mock directly | 1695023 | 0 | 112994 | 0.28 ms | 0.45 ms | | | | 33 |
| 32 | /v1/chat/completions | 562604 | 0 | 37505 | 0.85 ms | 1.15 ms | +0.57 ms | +0.70 ms | 99.4 MiB | 24 |
| 32 | /v1/chat | 567208 | 0 | 37812 | 0.84 ms | 1.14 ms | +0.56 ms | +0.69 ms | 99.4 MiB | 0 |
| 64 | mock directly | 1720036 | 0 | 114660 | 0.55 ms | 0.88 ms | | | | 65 |
| 64 | /v1/chat/completions | 549457 | 0 | 36628 | 1.74 ms | 2.24 ms | +1.19 ms | +1.36 ms | 104.2 MiB | 32 |
| 64 | /v1/chat | 545021 | 0 | 36332 | 1.75 ms | 2.20 ms | +1.21 ms | +1.32 ms | 104.5 MiB | 0 |

**Memory (resident set of the serve process):** 11.0 MiB idle before any load, 104.5 MiB after the last phase.

**Two earlier user/assistant turns** (`--history 2`):

| connections | path | requests | failed | req/s | p50 | p99 | added p50 | added p99 | peak RSS | upstream connections |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | mock directly | 344325 | 0 | 22953 | 0.04 ms | 0.06 ms | | | | 1 |
| 1 | /v1/chat/completions | 52805 | 0 | 3520 | 0.28 ms | 0.35 ms | +0.24 ms | +0.28 ms | 91.0 MiB | 1 |
| 1 | /v1/chat | 89359 | 0 | 5957 | 0.16 ms | 0.22 ms | +0.12 ms | +0.16 ms | 91.2 MiB | 0 |
| 8 | mock directly | 1454142 | 0 | 96936 | 0.08 ms | 0.14 ms | | | | 8 |
| 8 | /v1/chat/completions | 134479 | 0 | 8965 | 0.87 ms | 1.16 ms | +0.79 ms | +1.03 ms | 90.5 MiB | 1 |
| 8 | /v1/chat | 357078 | 0 | 23804 | 0.33 ms | 0.47 ms | +0.25 ms | +0.33 ms | 92.9 MiB | 6 |
| 32 | mock directly | 1619086 | 0 | 107932 | 0.29 ms | 0.47 ms | | | | 32 |
| 32 | /v1/chat/completions | 73205 | 699844 | 4880 | 2.40 ms | 2.79 ms | +2.11 ms | +2.31 ms | 96.8 MiB | 0 |
| 32 | /v1/chat | 543017 | 0 | 36199 | 0.87 ms | 1.22 ms | +0.58 ms | +0.75 ms | 99.8 MiB | 24 |
| 64 | mock directly | 1751515 | 0 | 116760 | 0.54 ms | 0.85 ms | | | | 64 |
| 64 | /v1/chat/completions | 70040 | 749204 | 4669 | 3.24 ms | 4.33 ms | +2.70 ms | +3.48 ms | 101.5 MiB | 0 |
| 64 | /v1/chat | 567617 | 0 | 37838 | 1.68 ms | 2.19 ms | +1.14 ms | +1.34 ms | 105.5 MiB | 57 |

**Memory (resident set of the serve process):** 11.0 MiB idle before any load, 105.5 MiB after the last phase.

### Reading it

- **The first-turn case is fixed.** The native `/v1/chat/completions` wire answered every
  request at 32 and 64 concurrent (0 failed of about 560,000 and 550,000), where the
  first and second runs refused 15 % and 27 %, and throughput there (37,500 and 36,600
  req/s) is the same as the gateway's own `/v1/chat`. Added latency over the mock is
  +0.57 ms at 32 and +1.19 ms at 64 (p50), equal to `/v1/chat`'s. Peak RSS after the
  last phase is 104 MiB, against 105 MiB for the second run's longer-lived process:
  the concurrent conversion 0023 allows did not show up in memory at this load (one
  small message; the memory gate in 0023 asks for a re-read with larger requests
  before a heavier workload is sized from it).
- **A caller that replays history is not helped, and is hurt worse than the first run
  suggested.** With two earlier turns the native wire is slow even at one connection
  (3,500 req/s against 6,200) and refuses **93 % of requests at 32 and 64**
  (699,844 of 773,049; 749,204 of 819,244), while `/v1/chat` is unaffected. This is
  what 0023 predicted from the code, not a regression: the Chat wire looks every
  assistant turn up in the receipt store (a `get` that takes the store's lock and
  reads its index), so it needs the executor and its two workers and one serial
  ingress worker, and everything beyond them is refused. The first run did not show it
  because it never sent an assistant turn.

### Status of the findings

2. *The replay-receipt executor refuses work when its slots are taken*: **resolved for
   receipt-free requests** (0023), **open for a caller that replays history**. A
   text-only assistant turn has no receipt to find, yet it pays for a locked store
   lookup. Ideas, none decided: answer the lookup from the store's in-memory index
   without the file lock when the receipt cannot exist (a negative check), skip the
   lookup for an assistant message that carries no reasoning and no tool calls (the
   wire writes a reasoning receipt only for those), or give the executor a bounded
   wait and more workers. Each changes what a replay can find, so it needs its own note.
