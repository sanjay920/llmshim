# 0021: The upstream HTTP client and listener limits from the file

Status: implemented. The library (`ClientSettings`, `ShimClient::from_settings`,
`Router::with_http_client`), the file's `[upstream]` and `[server]` keys built into the
router and the listeners, and the benchmark re-run, recorded in 0018 beside the first:
at 32 and 64 concurrent requests `/v1/chat` now answers every request at 37,000 to
39,000 per second with a handful of upstream connections, where the first run failed
and collapsed; an isolating run (the same binary, `--pool 4`) brings the collapse back
and shows a connection opened per request (16,000 in a phase). `serve` no longer reads `LLMSHIM_MAX_RETRIES`, `LLMSHIM_MAX_BACKOFF_SECS`,
`LLMSHIM_UPSTREAM_*`, `LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS` or `LLMSHIM_HTTP_*`. A contract
note, written before any code (README working rule 3).

## Why

`llmshim serve` takes every setting that belongs to the server from its file
(0001, 0002), and the last rows of the environment inventory that it still reads
are the settings of the one HTTP client every upstream call goes through, and the
two limits of its inbound listener. They matter more than their size suggests: the
benchmark (0018) found that the client keeps **4 idle connections per upstream
host** (a constant in `src/client.rs`), so above a handful of requests in flight
every extra request opens and closes its own connection and, on the benchmark
host, exhausted the ephemeral ports. That is a limit of HTTP/1.1 connections:
against a hosted provider that negotiates HTTP/2 (OpenAI, Anthropic and Gemini
over TLS) requests multiplex over a connection and the setting barely matters. It
bites on plain HTTP/1.1 upstreams, which is what a self-hosted vLLM or SGLang, an
OpenAI-compatible server, an HTTP proxy path or the benchmark's mock is. A fix that
is only an environment variable would repeat
the problem 0002 exists to remove: a process-wide setting a file cannot reach.

## The contract

```toml
[upstream]                         # how the gateway talks to providers
max_retries = 3                    # 0 to 10: retries of a transport error or a 429/5xx
max_backoff_secs = 60              # 1 to 300: the cap of the retry backoff
pool_max_idle_per_host = 256       # 1 to 4096: idle connections kept per host (see below)
pool_idle_timeout_secs = 90        # 1 to 3600
ambient_proxy = true               # honour HTTP_PROXY / HTTPS_PROXY / ALL_PROXY
connect_timeout_ms = 30000         # each deadline: positive and finite (the library's rule)
response_header_timeout_ms = 600000
policy_callback_timeout_ms = 10000
error_body_idle_timeout_ms = 10000
error_body_total_timeout_ms = 30000
unary_idle_timeout_ms = 300000
unary_attempt_timeout_ms = 1800000
stream_idle_timeout_ms = 300000
stream_attempt_timeout_ms = 7200000

[server]
max_connections = 1024             # 1 to 65535: inbound connections held at once
header_timeout_ms = 15000          # 1 to 600000: time allowed for a request's headers
```

Every key is optional. The values shown are the library's current defaults
(`AttemptDeadlines::default`, `RetryConfig::default`, the listener's limits) except
`pool_max_idle_per_host` (see below), and a test pins that an empty `[upstream]`
table equals them, so this note cannot drift from the code it describes. A deadline
has no upper bound beyond being finite, because the library has none; the bounds on
the other keys are this note's.

## Rules

- **The file is the only source.** `serve` stops reading `LLMSHIM_MAX_RETRIES`,
  `LLMSHIM_MAX_BACKOFF_SECS`, `LLMSHIM_UPSTREAM_*`, `LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS`
  and `LLMSHIM_HTTP_*`. The test harness already sets canary values for the ones
  that would break a request, and gains these. The proxy and gateway keep reading
  them; their behaviour does not change.
- **A client owned by the router, not a process global.** Today every call goes
  through `SHARED_CLIENT`, built once from the environment. This note adds, to the
  public library, a defaulted-field `Router::with_http_client(ShimClient)` and a
  `ShimClient` constructor from explicit settings that **never reads the
  environment**. `bound_client` uses the router's client when it has one and is
  otherwise exactly what it is now, so no other build changes. This is an additive
  change to `router.rs` and `client.rs`, to be released with a minor bump.
  **A scoped router wins.** A router that forbids ambient proxies
  (`from_scoped_credentials`, `without_ambient_proxy`) never forwards its
  credentials through a host-chosen proxy; `bound_client` checks at use that the
  router's client agrees (a scoped router with an ambient-proxy client is an error,
  not a silent downgrade), so the order the two builders are called in does not
  matter.
- **The cloud transports' clients get the pool and connect settings too.** Bedrock,
  Vertex and Azure do not have clients of their own: they ask for the client's
  *scoped* transport (`ShimClient::scoped_http`, built lazily, no ambient proxy,
  today with the same hard-coded 4 idle connections). The pool, idle timeout and
  connect settings reach that transport as well (the builder takes the settings, not
  only a connect timeout), and `ambient_proxy` does not apply to it (it is never
  ambient). This corrects 0020, which said they keep their own clients.
- **The pool default for `serve` follows the concurrency it serves.** Idle
  connections cost little and the idle timeout reaps them, so the pool should be at
  least the most requests in flight to one host. When `pool_max_idle_per_host` is
  not set it is the larger of 256 (the default concurrency per provider,
  `GatewayConfig::max_concurrency_per_provider`) and `limits.max_concurrency` (one
  value for every provider; there is no per-provider concurrency key), capped at 4096;
  a value the file sets is used
  as written, and the note in the file's schema says it should not be below the
  largest per-host total (several providers can share a host). The library's own
  default stays 4, because changing what every embedding application does is a
  separate decision, recorded here and left to the owner.
- **Validation.** The deadlines are checked by `AttemptDeadlines::validate`, the
  same function the client applies, so `--check` and the client cannot disagree (as
  0020 does for the cloud rules). That function only requires each value to be
  positive and finite, which would let a file make a deadline unreachable, so the
  file adds ordering rules, by path: `connect_timeout_ms` and
  `response_header_timeout_ms` may not exceed `unary_attempt_timeout_ms` or
  `stream_attempt_timeout_ms`, `unary_idle_timeout_ms` not `unary_attempt_timeout_ms`,
  `stream_idle_timeout_ms` not `stream_attempt_timeout_ms`, and
  `error_body_idle_timeout_ms` not `error_body_total_timeout_ms` (the defaults
  satisfy them). The retry, backoff, pool and listener bounds above are enforced by
  the file and by the constructors.
- **`ambient_proxy` defaults to `true`**, which is what `serve` does today (the
  standard proxy variables apply to the shared client, 0002/0004). Setting it to
  `false` builds a client that ignores them, for a deployment that must not route
  provider traffic through a proxy a host happens to export. It does not affect the
  cloud transports, whose transport is always scoped (above).
- **Fixed at start.** These are outside the keys, so a reload that changes one is
  refused by path (0012).
- **The inbound limits move with it.** `[server] max_connections` and
  `header_timeout_ms` replace `LLMSHIM_HTTP_MAX_CONNECTIONS` and
  `LLMSHIM_HTTP_HEADER_TIMEOUT_MS` for `serve`; the listener takes them as
  arguments (the environment reader stays for the proxy and gateway).
- **The benchmark is the acceptance test, and it gets the measurement it lacked.**
  0018's cause rests on a hand-read `TIME_WAIT` count. Slice 4 first gives the
  harness a count of the connections the mock accepted in each phase (a counting
  listener, so it works wherever the harness runs), then re-runs with the new
  default and records both runs side by side. Failures that stop at 32 and 64
  concurrent requests *and* a connection count that stays near the concurrency
  resolve the finding; if either does not happen, the explanation in 0018 was wrong
  or incomplete and that is recorded instead.

## Not decided here

- The library's own `pool_max_idle_per_host` default (4), and a public setting on
  the shared client for embedding applications.
- Per-provider client settings (one pool per provider host with its own limits).
- The replay-receipt executor's capacity, the benchmark's other finding. It is a
  different component with its own question (whether an answer with no reasoning or
  tool block needs a receipt).

## Slices

1. This note.
2. Done: the library: explicit `ClientSettings` and the `ShimClient` constructor that
   never reads the environment, `Router::with_http_client`, `bound_client` using
   it (and refusing a scoped router with an ambient client), and the scoped
   transport taking the same settings. (The listener taking its limits as arguments
   is part of slice 3: it is in the binary, not the library.) Tests:
   a settings value builds the client it describes (pool, deadlines, retry, on both
   the shared and the scoped transport); a client built from
   settings ignores every `LLMSHIM_*` canary in the environment; a router without a
   client behaves as before; the empty-table-equals-defaults pin.
3. Done: the configuration: the `[upstream]` and `[server]` keys, their validation,
   the schema, building the router's client from them, and process tests (a setting
   takes effect, a canary variable does not, an invalid value is refused by path,
   `--check` agrees).
4. Done: the harness's per-phase accepted-connection count; the benchmark re-run, recorded
   in 0018 beside the first; the rows of 0002; the roadmap; and a pointer from 0020's
   "not decided" entry on an inference-only proxy switch to `[upstream] ambient_proxy`.
