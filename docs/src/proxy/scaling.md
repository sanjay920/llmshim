# Scaling and rate limits

The proxy is stateless per request. Put replicas behind a load balancer and
send the complete conversation history on every call. No sticky session is
required for llmshim itself.

```mermaid
flowchart LR
    C[Clients] --> L[Load balancer]
    L --> P1[Proxy replica 1<br/>pool + admission state]
    L --> P2[Proxy replica 2<br/>pool + admission state]
    L --> P3[Proxy replica N<br/>pool + admission state]
    P1 --> U[Provider APIs]
    P2 --> U
    P3 --> U
    P1 -. optional shared limits .-> R[(Redis)]
    P2 -. optional shared limits .-> R
    P3 -. optional shared limits .-> R
```

Each process owns its connection pool, concurrency semaphore, and—unless
Redis coordination is enabled—its rate-limit buckets.

## Connection reuse and warmup

All calls in one process share a lazily initialized HTTP client. Its pool uses
HTTP/2 where available, gzip/brotli/zstd/deflate compression, a 90-second idle
timeout, up to four idle connections per host, a 30-second TCP keepalive, and
TCP `NODELAY`.

For Rust services, call `llmshim::warmup(&router).await` after constructing the
Router. It sends bounded HEAD requests to configured built-in provider origins
to pre-establish TCP and TLS connections. Failure to warm a provider is
ignored; normal request handling remains authoritative.

## Reactive retries

The shared client retries transport failures and HTTP `429`, `500`, `502`,
`503`, `504`, and `529`. The default is three retries after the initial
request—up to four attempts total—with exponential backoff and jitter.
`Retry-After` and recognized OpenAI or Anthropic reset headers take precedence
over computed backoff. Invalid reset durations are ignored; oversized finite
durations saturate safely before the configured cap and jitter are applied.

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_RETRIES` | `3` | Retries after the initial attempt |
| `LLMSHIM_MAX_BACKOFF_SECS` | `60` | Cap for any one wait |

These retries stay on the same route. A non-streaming fallback chain adds a
second layer: **retry the route, then change the route**. See
[Fallback chains](../guides/fallbacks.md).

## Backpressure and proactive limits

Every actual provider attempt acquires an instance concurrency slot immediately
before the send. Waiting longer than the queue timeout returns `503` with
`Retry-After`. A retry or repair releases the completed attempt's slot and must
acquire again; fallback acquires against the provider it actually targets.

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_CONCURRENCY` | `256` | Maximum in-flight upstream requests per replica |
| `LLMSHIM_QUEUE_TIMEOUT_MS` | `5000` | Maximum wait for a slot |

Optional token buckets can reject work before it reaches a provider. A
rejection is `429` with `Retry-After`.

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_RATE_LIMIT_RPM` | unset | Requests per minute, used as the per-provider default |
| `LLMSHIM_RATE_LIMIT_TPM` | unset | Estimated tokens per minute, used as the per-provider default |
| `LLMSHIM_<PROVIDER>_RPM` | unset | Override RPM for `OPENAI`, `ANTHROPIC`, `GEMINI`, or `XAI` |
| `LLMSHIM_<PROVIDER>_TPM` | unset | Override TPM for that provider |
| `LLMSHIM_PENALTY_SECS` | `5` | Bucket penalty after an upstream `429` |

When neither RPM nor TPM is set, proactive rate limiting is disabled;
concurrency backpressure still applies. Token permits are estimates based on
the final provider-native body and authoritative prepared target. They include
the serialized native prompt and schema material, recognized native reasoning
budgets, and the effective native output limit.
For native Chat Completions bodies, OpenAI's `n` is treated as a generated
candidate count; Gemini `generationConfig.candidateCount` is treated the same
way. The output allowance is multiplied by the final wire candidate count. The
estimator also defensively recognizes `best_of`, `bestOf`, and
`candidate_count`, including a `generation_config` container that llmshim can
preserve from `x-gemini`, when a final body contains them; that compatibility
handling does not imply universal provider support.
Anthropic's omitted output limit uses
the adapter's 8,192-token default; other known models use the catalog output
ceiling when the provider leaves the limit unspecified. Unknown hosted models
use a conservative provider-family ceiling; unknown self-hosted models retain
the 1,024-token fallback because the server's launch configuration is not
visible to llmshim. OpenRouter `models` fallbacks remain supported; when no
explicit output limit is present, every listed model contributes its catalog
ceiling and an unknown listed model uses a one-million-token ceiling.

These permits are conservative estimates, not provider billing measurements.
The input side uses serialized characters divided by four; provider tokenizers,
images, caching, and unknown self-hosted model defaults can differ. Admission
occurs once per actual network attempt. Transport retries, managed schema
repairs, and supported fallback targets each reacquire RPM and TPM from the lane
for the provider/body that will be sent. A provider-wide refusal may advance a
fallback chain only to a distinct provider; an authenticated tenant refusal
terminates the request.

For an authenticated gateway identity, an omitted `rpm` or `tpm` field means
that dimension is unlimited. An explicit `0` means that dimension admits no
requests; the gateway rejects before dispatch and does not consume the other
tenant bucket. The same deny-all behavior applies to global and per-provider
rate-limit values: an explicit zero returns `429` before dispatch, while an
omitted value remains unlimited.

## Provider health

Rate limiting and health are different questions. A `429` means the provider is
alive and asking for less, and the token buckets already slow it down. A
circuit breaker counts what retrying cannot fix — `500`, `502`, `503`, `504`,
`529` and transport failures — over a sliding window, opens the circuit at the
threshold, and admits a single probe after the cooldown.

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_BREAKER_WINDOW_SECS` | `60` | Sliding window over which failures are counted |
| `LLMSHIM_BREAKER_TRIP_THRESHOLD` | `3` | Failures that open a circuit; `0` disables the breaker |
| `LLMSHIM_BREAKER_COOLDOWN_SECS` | `30` | Time an open circuit waits before admitting a probe |

Every dispatch path *observes* outcomes, so health accrues from ordinary
traffic. Only a [fallback chain](../guides/fallbacks.md) *refuses*: it skips a
provider with an open circuit instead of spending its retry budget on a target
it already knows is dead. A single-target request is still dispatched — with no
alternative, refusing would only convert an upstream failure into a local one.

## Spend caps

The experimental gateway enforces a per-identity USD cap beside the RPM/TPM
buckets. A gateway key's identity may carry `budget_usd` and an optional
`budget_window_secs` (default one day); over budget is a `429` with
`Retry-After` set to the window reset.

```json
{"sk-example": {"tenant": "acme", "tier": 1, "budget_usd": 100, "budget_window_secs": 86400}}
```

Cost is only knowable after a response, so the cap is checked before dispatch and
charged after. Everything admitted between the last charge and the next check
passes, so the overshoot bound is **admitted concurrency × the most expensive
request**, multiplied again across replicas that have not yet shared their
ledger. Size a cap with that headroom in mind rather than as a hard ceiling.

The current spend ledger charges successful returned usage. A provider response
that is later discarded by a failed managed repair, and a send whose billing is
uncertain after a transport failure or worker loss, can still escape settlement.
Treat this as a soft accounting cap until per-attempt reservation and settlement
are enabled; RPM/TPM attempt coordination does not close that spend gap.

A response the catalog cannot price at all is **not** charged — recording zero
would let an unpriced model run forever under a budget. A model that prices only
*some* token classes is charged at its highest published rate for the rest, so a
partial price bounds the charge from above instead of voiding it: 2,537 of the
7,461 priced models in the catalog publish no `cache_read` rate, and voiding
those would have reopened this same hole one layer down. So that a cap cannot silently stop
binding, a request whose target has **no catalog price is refused before it runs**
when a budget is set:

```
400 {"error":{"code":"unpriceable_under_budget","param":"model", …}}
```

It is deliberately not a `429`: retrying never clears it. Three ways forward —
use a priced model, add a local price override in the catalog, or accept the risk
explicitly per key:

```json
{"sk-example": {"tenant": "acme", "budget_usd": 100, "budget_allow_unpriced": true}}
```

`budget_allow_unpriced` defaults to `false`. With it set, those requests run and
are not charged, and each one logs a warning and increments
`llmshim_gateway_unpriced_under_cap_total{provider,model}` — a non-zero counter
means the budget is not binding for that target. An accepted risk should stay
measurable rather than become an assumption.

## One replica or a coordinated fleet

The default buckets are in memory. With `N` replicas, each replica enforces
its own configured limit. If the number represents a fleet-wide provider
quota, divide it across instances or enable shared coordination.

To share one bucket, build the opt-in feature and set Redis:

```bash
cargo install llmshim --features redis-coordination
LLMSHIM_REDIS_URL=redis://redis.internal:6379 llmshim proxy
```

`redis-coordination` includes the `proxy` feature. Redis coordinates provider
rate-limit buckets, provider health and — on the authenticated gateway — tenant
RPM/TPM and spend, so these limits mean the same thing on every replica. The
gateway checks provider and tenant RPM/TPM in one Lua decision: if any dimension
rejects, none is debited. Its trusted per-attempt coordinator fails closed when
Redis is unavailable. The compact proxy's standalone Redis limiter keeps its
documented fail-open behavior. Both paths share the same provider bucket keys.
Connection pools and concurrency limits remain per process. If the Redis client
cannot be initialized—or the binary lacks the feature—the compact proxy warns
and falls back to in-memory buckets.

With `gateway-redis`, admitting a new job checks the provider's waiting-queue
depth and inserts the job in one Lua transaction. Concurrent origins cannot
claim the same remaining slot. `LLMSHIM_GATEWAY_QUEUE_DEPTH` defaults to 10,000
waiting jobs per provider; a full queue refuses new work with `503` and
`Retry-After`.

Do not infer capacity from llmshim's implementation details alone. The
[README benchmarks](https://github.com/sanjay920/llmshim#benchmarks) are the
maintained performance snapshot; load-test your model mix, payload sizes,
provider quotas, and gateway before choosing replica counts.
