# 0021: The upstream HTTP client and listener limits from the file

Status: design only. A contract note, written before any code (README working
rule 3). Nothing here is implemented; the slices are listed at the end.

## Why

`llmshim serve` takes every setting that belongs to the server from its file
(0001, 0002), and the last rows of the environment inventory that it still reads
are the settings of the one HTTP client every upstream call goes through, and the
two limits of its inbound listener. They matter more than their size suggests: the
benchmark (0018) found that the client keeps **4 idle connections per upstream
host** (a constant in `src/client.rs`), so above a handful of requests in flight
every extra request opens and closes its own connection and, on the benchmark
host, exhausted the ephemeral ports. Against a real provider the same churn is a
TLS handshake per request. A fix that is only an environment variable would repeat
the problem 0002 exists to remove: a process-wide setting a file cannot reach.

## The contract

```toml
[upstream]                         # how the gateway talks to providers
max_retries = 3                    # 0 to 10: retries of a transport error or a 429/5xx
max_backoff_secs = 60              # 1 to 300: the cap of the retry backoff
pool_max_idle_per_host = 256       # 1 to 4096: idle connections kept per provider host
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
- **The pool default for `serve` is 256, not 4.** 256 is the default concurrency per
  provider (`GatewayConfig::max_concurrency_per_provider`), so in steady state no admitted request opens a
  connection of its own; idle connections cost little and the idle timeout reaps
  them. The library's own default stays 4, because changing what every embedding
  application does is a separate decision, recorded here and left to the owner.
- **Validation is the library's.** The deadlines are checked by
  `AttemptDeadlines::validate`, the same function the client applies, so `--check`
  and the client cannot disagree (as 0020 does for the cloud rules); the retry,
  backoff and pool bounds above are enforced by the file and by the constructor.
- **`ambient_proxy` defaults to `true`**, which is what `serve` does today (the
  standard proxy variables apply to the shared client, 0002/0004). Setting it to
  `false` builds a client that ignores them, for a deployment that must not route
  provider traffic through a proxy a host happens to export. It does not affect the
  cloud transports, which keep their own clients (0020).
- **Fixed at start.** These are outside the keys, so a reload that changes one is
  refused by path (0012).
- **The inbound limits move with it.** `[server] max_connections` and
  `header_timeout_ms` replace `LLMSHIM_HTTP_MAX_CONNECTIONS` and
  `LLMSHIM_HTTP_HEADER_TIMEOUT_MS` for `serve`; the listener takes them as
  arguments (the environment reader stays for the proxy and gateway).
- **The benchmark is the acceptance test.** The change is not done until the
  benchmark (0018) is re-run with the new default and the result recorded beside
  the first run, whatever it shows: if connections stop failing at 32 and 64
  concurrent requests the finding is resolved; if they do not, the explanation in
  0018 was wrong, and that is recorded instead.

## Not decided here

- The library's own `pool_max_idle_per_host` default (4), and a public setting on
  the shared client for embedding applications.
- Per-provider client settings (one pool per provider host with its own limits).
- The replay-receipt executor's capacity, the benchmark's other finding. It is a
  different component with its own question (whether an answer with no reasoning or
  tool block needs a receipt).

## Slices

1. This note.
2. The library: explicit `ClientSettings` and the `ShimClient` constructor that
   never reads the environment, `Router::with_http_client`, `bound_client` using
   it, and the listener taking its limits as arguments. Tests: a settings value
   builds the client it describes (pool, deadlines, retry); a client built from
   settings ignores every `LLMSHIM_*` canary in the environment; a router without a
   client behaves as before; the empty-table-equals-defaults pin.
3. The configuration: the `[upstream]` and `[server]` keys, their validation,
   the schema, building the router's client from them, and process tests (a setting
   takes effect, a canary variable does not, an invalid value is refused by path,
   `--check` agrees).
4. The benchmark re-run, recorded in 0018; the rows of 0002; the roadmap.
