# 0016: OpenTelemetry traces

Status: slices 1 to 3 implemented (the request span and the exporter); the
attempt span and the log writers' counters remain. A contract note, written
before any code (README working rule 3).

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

**2. OTLP over HTTP with protobuf, no gRPC.** Every common collector accepts
OTLP/HTTP on `:4318`; tonic and the gRPC stack do not enter the tree.
**A fact that changes the plan:** `opentelemetry-http` and `opentelemetry-otlp`
0.33 require `reqwest ^0.13.1`, and this crate is on reqwest 0.12 (checked on
crates.io, 2026-11-15). Using the OTLP crate as it ships would put a second
reqwest, with its own TLS configuration, into the tree. Two ways out, decided by
a measurement in slice 2 and not by taste:

- **(a)** the `opentelemetry-otlp` crate with `http-proto` and reqwest 0.13, only
  under `serve-otel`, accepted only if `cargo tree -i tonic` and
  `cargo tree -i native-tls` are empty and its TLS is rustls with the platform
  trust store, so a second client does not mean a second trust configuration;
- **(b)** a small `SpanExporter` of our own that encodes the OTLP protobuf
  messages and posts them with the reqwest the crate already has. More code to
  own; no second HTTP stack.

If (a) fails its check, (b). Upgrading the crate's own reqwest to 0.13 is not
considered here: it would change every build.

**3. A sub-feature, so no other build changes.** `serve-otel = ["serve",
"dep:opentelemetry", ...]`, default off and implied by nothing, like
`serve-redis`. A file with a `[telemetry]` table, built without it, is a startup
error naming the rebuild flag (`feature_issues`), never a silently ignored
setting. Default, `proxy`, `gateway`, `gateway-redis` and plain `serve` do not
gain a dependency. The packages added to `Cargo.lock` are listed in the pull
request that adds them.

**4. No `tracing` crate.** The OpenTelemetry API is called directly at a few
places, so the code does not acquire a global subscriber the rest of the program
would then have to live with. The SDK's own internal logs (a dropped batch, a
failed export) are routed to the stderr reporter of decision 8, or disabled; no
subscriber is installed to receive them.

**5. What is a span.**

| Span | Starts | Ends | Notable attributes |
| --- | --- | --- | --- |
| `llmshim.request` (server) | the access-log layer receives the request | the response headers are sent (as 0009 logs it), or the request is abandoned, in which case the span ends with an explicit `abandoned` status, as the access line has `status: null` | `http.request.method`, `http.route` (the path without query), `http.response.status_code`, `llmshim.request_id`, `llmshim.tenant`, `llmshim.key` (the key's configured name; a credential nobody recognises has none, and its fingerprint is not an attribute) |
| `llmshim.attempt` (client, child of the request) | `UsageRecorder::begin` | `UsageRecorder::finish` | `llmshim.provider`, `llmshim.model`, `llmshim.attempt.kind`, `llmshim.attempt.outcome`, token counts, `llmshim.cost_usd`, `llmshim.cost_source` |

The attempt span is driven from the same two seams, and carries the same record,
as the usage log (0010): one span per admitted attempt, none for a refusal,
`abandoned` when dropped. **A stream's attempt outlives its request span**, which
ends at the response headers; a child that ends after its parent is legal in
OpenTelemetry, and the stream's duration lives only on the attempt span. Cost is `null`-means-unknown here too: the attribute is absent, never
`0`. The queue wait is a span event (`queued`, `dispatched`), not a span.

**6. What is never an attribute.** A token, a header value, a query string, a
prompt, a completion, a tool argument, an error body, a secret reference's value.
The attribute set above is closed and a test pins it: a new attribute is a
deliberate edit in the test and in this note. Key names and tenant names appear
because the access log already carries them; an operator who does not want them
in a collector can drop the attributes there.

**7. Propagation.** An inbound W3C `traceparent` is honored **only on an
authenticated request** (a caller the key store recognises): the request span
joins the caller's trace, which is what lets a caller's own trace show the
gateway's part. An unauthenticated or malformed one is ignored and a new trace is
started, so a stranger cannot choose a trace id or force the sampler. **`traceparent` is not forwarded to providers.** Provider calls
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

`otlp_endpoint` goes through the same URL check as `base_url` (and plain `http`
is accepted, since a collector on a private network or a sidecar often has no
TLS; the operator chooses); header values are
secret references in the form 0001 defines (`{ env = "NAME" }` or
`{ file = "/path" }`), never echoed, and absent from the checked-in schema's
examples and from `/v1/serve/config`; a `sample_ratio` outside 0 to 1 is a named
error (`telemetry.sample_ratio`); `sample_ratio` is applied as a
parent-based ratio so a caller's decision is respected. A TLS-terminating
collector uses the platform trust store; client-certificate export is not in the
first slice. `--check` validates the table and, as for every secret, reports an
unavailable header value with status 3; `--structure-only` reads none of it.

**10. Reload.** `[telemetry]` is fixed at start, like every table but `[[keys]]`
(0012): a change is refused by path.

**11. A fleet.** An attempt can run on a replica other than the one that took the
request. Joining its span to the request's trace needs the trace and parent span
ids in the Redis job scope, which is a wire change to a scope a test pins, and it
needs a compatibility rule for a mixed-version fleet (an old replica ignores the
fields; a job queued by one has none, as with the key name and request id in
0010). **The first slices do not do this**: a fleet attempt span is emitted on
the replica that runs it, carries the request id as an attribute, and is not
linked to the request span. Its tenant and key attributes come from that
replica's own tables (the grow-only rule of 0010), so they can be absent. Linking
is a later slice with its own transition note.

## Slices

1. This note. Done.
2. `[telemetry]` in the config contract and `serve-otel` feature gating:
   validation, the schema, `feature_issues`, `--check`; no exporter yet. Done:
   until slice 3 the table is valid but refused at startup in every build (with
   `serve-otel` the message says it is not implemented yet), so a file cannot say
   traces leave when none do. Because of that refusal, the status-3 path for an
   unavailable header value is only reachable once slice 3 lifts it; slice 3 adds
   the command-line test for it (this slice pins it in a unit test).
3. The request span and propagation; the exporter; the bounded processor; the
   drop and failure counters in `/metrics`; an end-to-end test against a local
   OTLP/HTTP receiver, including a collector that is down and one that stalls.
   Done, with these choices made on the evidence: **option (b)** of decision 2
   (the OTLP crate needs reqwest 0.13, and 0.13's `rustls` brings aws-lc-rs; the
   exporter is ours, over the reqwest the crate has; eight packages join
   `Cargo.lock` under `serve-otel`, with no tonic, no aws-lc and no second
   reqwest); a bounded queue of 2048 spans drained by a worker thread (batches of
   256, a partial batch after 5 s, a 10 s export deadline, a 5 s shutdown bound)
   in place of the SDK's batch processor, so that a drop is counted:
   `llmshim_telemetry_spans_dropped_total` and
   `llmshim_telemetry_export_failures_total`. The resource is built without the
   SDK's environment detectors (`OTEL_*` is not read). Only the request span
   exists; the exporter's queue, batch and timeout sizes are constants, not
   settings.
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
