# 0016: OpenTelemetry traces

Status: proposed. A contract note, written before any code (README working rule
3). Nothing here is implemented; it is the design the next slices build, so that
review can argue with the decisions before they cost anything.

## What an operator needs

Step 4 gave every request one id that joins its response, its access-log line and
its usage records. A trace is the same join with timing: which request, which
attempts (a retry, a repair, a fallback), how long each waited for capacity and
for the provider, and where it failed. An enterprise's collector already speaks
OTLP; `serve` should send it spans and nothing else.

## Decisions

**1. Traces only. `/metrics` stays the one metrics surface.** `serve` already
serves Prometheus text (`gateway::metrics`, dependency-free) and 0015 holds it to
the OpenAPI document. A second metrics pipeline (OTLP metrics) would give two
sources of truth for the same counters and drag in the SDK's metric machinery.
The gaps that exist are added to `/metrics` instead: the log writers' dropped and
failed line counts (a follow-up in the roadmap), and exporter drops (below).
OTLP metrics can be revisited if an operator needs them; it is not refused, it is
not now.

**2. OTLP over HTTP with protobuf, no gRPC.** `opentelemetry` 0.33,
`opentelemetry_sdk` and `opentelemetry-otlp` with `http-proto` over the `reqwest`
the crate already uses. tonic, prost's gRPC stack and a second HTTP client do not
enter the tree. Every common collector accepts OTLP/HTTP on `:4318`.

**3. A sub-feature, so no other build changes.** `serve-otel = ["serve",
"dep:opentelemetry", ...]`, default off and implied by nothing, like
`serve-redis`. A file with a `[telemetry]` table, built without it, is a startup
error naming the rebuild flag (`feature_issues`), never a silently ignored
setting. Default, `proxy`, `gateway`, `gateway-redis` and plain `serve` do not
gain a dependency. The packages added to `Cargo.lock` are listed in the pull
request that adds them.

**4. No `tracing` crate.** The OpenTelemetry API is called directly at three
places, so the code does not acquire a global subscriber the rest of the program
would then have to live with.

**5. What is a span.**

| Span | Starts | Ends | Notable attributes |
| --- | --- | --- | --- |
| `llmshim.request` (server) | the access-log layer receives the request | the response headers are sent (as 0009 logs it) | `http.request.method`, `http.route` (the path without query), `http.response.status_code`, `llmshim.request_id`, `llmshim.tenant`, `llmshim.key` |
| `llmshim.attempt` (client, child of the request) | an attempt is admitted | it ends or is abandoned | `llmshim.provider`, `llmshim.model`, `llmshim.attempt.kind`, `llmshim.attempt.outcome`, token counts, `llmshim.cost_usd`, `llmshim.cost_source` |

The attempt span uses the same boundaries, and the same record, as the usage log
(0010): one span per admitted attempt, none for a refusal, `abandoned` when
dropped. Cost is `null`-means-unknown here too: the attribute is absent, never
`0`. The queue wait is a span event (`queued`, `dispatched`), not a span.

**6. What is never an attribute.** A token, a header value, a query string, a
prompt, a completion, a tool argument, an error body, a secret reference's value.
The attribute set above is closed and a test pins it: a new attribute is a
deliberate edit in the test and in this note. Key names and tenant names appear
because the access log already carries them; an operator who does not want them
in a collector can drop the attributes there.

**7. Propagation.** An inbound W3C `traceparent` is honored: the request span
joins the caller's trace, which is what lets a caller's own trace show the
gateway's part. **`traceparent` is not forwarded to providers.** Provider calls
are another organization's endpoint, and a trace id there would let them correlate
requests we sent on behalf of different callers. `x-request-id` is already
settled by the outermost layer (0009) and the span carries it.

**8. Telemetry fails open.** The rule of 0013 bar item 2 applies: a collector
that is down, slow or wrong never blocks or fails a request. Spans go to a bounded
batch processor on its own thread; when the queue is full a span is dropped and
counted, a failed export is counted, and the first of each is reported on stderr,
as the log writers do. Both counters are `/metrics` series. Shutdown flushes what
is queued, within a bound, and does not hold the process.

**9. Configuration** (a new top-level table, strict like the rest):

```toml
[telemetry]
otlp_endpoint = "https://collector.internal:4318"   # required; http(s)
service_name  = "llmshim"                            # default
sample_ratio  = 1.0                                  # 0 to 1, parent-based
[telemetry.headers]                                  # e.g. an auth token
authorization = { env = "OTLP_AUTHORIZATION" }       # secrets by reference only
```

`otlp_endpoint` goes through the same URL check as `base_url`; header values are
secret references, never echoed (0001); `sample_ratio` is applied as a
parent-based ratio so a caller's decision is respected. A TLS-terminating
collector uses the platform trust store; client-certificate export is not in the
first slice. `--check` validates the table and, as for every secret, reports an
unavailable header value with status 3; `--structure-only` reads none of it.

**10. Reload.** `[telemetry]` is fixed at start, like every table but `[[keys]]`
(0012): a change is refused by path.

## Slices

1. This note.
2. `[telemetry]` in the config contract and `serve-otel` feature gating:
   validation, the schema, `feature_issues`, `--check`; no exporter yet.
3. The request span and propagation; the exporter; the bounded processor; the
   drop and failure counters in `/metrics`; an end-to-end test against a local
   OTLP/HTTP receiver, including a collector that is down and one that stalls.
4. The attempt span from the attempt hooks; the closed attribute set pinned by a
   test; the trace joins the request id (closing the roadmap line).
5. The log writers' drop and failure counters as `/metrics` series.

## Not decided here

- The metric names for exporter drops (they follow the existing `llmshim_`
  naming and are decided in slice 3 with the OpenAPI conformance test, 0015, which
  does not yet describe metric families).
- Whether a second export protocol is ever wanted. gRPC is not refused; it is
  expensive and nobody has asked.
- Logs over OTLP. The access and usage logs stay JSON lines.
