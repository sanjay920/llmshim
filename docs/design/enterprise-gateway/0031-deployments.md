# 0031: Deployments: one name, several targets

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a file
table, a model-string prefix, an allowlist rule, a usage-record field and a failure behavior become
promises. It is the roadmap item "deployments and load balancing across credentials and regions".
Nothing here is started.

## The problem

A provider entry is one credential at one address (`[providers.openai-a]` with one key, one
`base_url`). Production needs several: two accounts to double a rate limit, a second region, a
fallback account. Today a caller must name one of them in the model string, so spreading load or
surviving a bad account means the caller changing its own configuration. The named routes of 0001
map a name to **one** model plus settings and deliberately "do not chain"; there is no name that means
"any of these".

## What exists, and why it shapes the design

Read from `src/router.rs`, `src/breaker.rs`, `src/gateway/` and `src/serve/`:

- **Admission is keyed by the resolved provider.** Rate buckets, the concurrency cap, the circuit
  breaker, the queue lane (one per provider in `Scheduler` and in Redis) and the usage record all use the
  provider key `Router::resolve_key` returns. A request is therefore best balanced **before** admission,
  by resolving the name to a concrete target; after that it is an ordinary request to that provider,
  with every limit and budget applying to the account it will use.
- **Budgets are per tenant, not per provider**, so balancing across targets does not split a budget.
- **The breaker counts failures per provider** (5xx and transport errors, not 429s) and only
  `fallback.rs` (the proxy's chain) refuses a target it has opened; the gateway dispatches a
  single-target call even to an open circuit, because refusing would only convert an upstream failure
  into a local one.
- **The gateway has no fallback.** `fallback` is read by the proxy's handlers, never by
  `gateway::http`. A job is queued in its provider's lane, so moving a request to another provider
  means a new submission, not a retry inside the job.
- `config::Route` is a public struct (`src/config.rs`) whose extra keys flatten into `settings`:
  adding a field to it is a semver change and would collide with a setting named `targets`.
  A deployment is therefore a **new** type, not a change to `Route`.

## The rule

```toml
[providers.openai-a]   # one account
kind = "openai"
api_key = { env = "OPENAI_KEY_A" }
[providers.openai-b]   # a second account, or a second region by base_url
kind = "openai"
api_key = { env = "OPENAI_KEY_B" }
base_url = "https://eu.example/v1"

[deployments.chat]
strategy = "weighted"          # "round_robin" (default) | "weighted" | "ordered"
targets = [
  { model = "openai-a/gpt-6-luna", weight = 3 },
  { model = "openai-b/gpt-6-luna", weight = 1 },
]
```

- **Addressed as `deployment/<name>`**, exactly as a route is addressed `route/<name>`: through the
  existing `provider/model` grammar, so an OpenAI SDK, the CLI and the proxy's admission handle it
  without learning a field. The prefix is reserved; a deployment name follows the route-name grammar.
  Per-request settings still win. A deployment carries **no settings of its own** (a route is the
  place for default settings), and **routes and deployments do not chain** either way: a deployment
  target is a concrete `provider/model`, never a `route/` or `deployment/` string.
- **Selection happens once per request, at resolution, before anything is admitted.** The chosen
  target replaces the name, and from then on the request is indistinguishable from one that named the
  target: its provider's limits, lane, breaker and the key's allowlist and budgets all see the real
  target.
- **Strategies** (all per replica; replicas balance independently, see "Not decided"):
  - `round_robin`: each target in turn.
  - `weighted`: smooth weighted round robin (nginx's algorithm): over any window of `sum(weights)`
    requests a target is chosen exactly `weight` times, evenly interleaved; weights are 1 to 1000.
  - `ordered`: the first target whose circuit is not open, in file order (primary and standby). When
    every circuit is open the first target is chosen anyway: refusing locally would only turn an upstream
    failure into a local one, the existing single-target rule.
- **Health.** `round_robin` and `weighted` skip a target whose provider's circuit is **open** (not
  half-open) and spread its share over the rest; when every target is open they ignore health. Health
  is the breaker's (5xx and transport failures); a rate-limit 429 is not an outage and does not skip a
  target (0010's rule that a 429 means the provider is alive).
- **Validation (by path).** At least one and at most 16 targets; each `model` must be `provider/model`
  with a configured `[providers.*]` provider and **not** a `route/` or `deployment/` string; a target
  listed twice is refused (a weight is how to prefer one); `weight` only with `weighted`; the name
  follows the route grammar and must not collide with a route's (separate prefixes, but one namespace
  keeps a log line unambiguous). Config-time checks cannot know a model exists upstream, as for routes.
- **Allowlists (0028).** A key's `allowed_models` is matched against the **chosen target's** routing id,
  and `deployment/<name>` (or `deployment/*`) admits the name the way `route/<name>` does. So a key
  allowed `deployment/chat` but only some of its targets would be refused when the pick lands on an
  unlisted one: a key that lists the name is allowed **all** its targets (the operator wrote the
  deployment), and a key that lists targets must list every target it may be sent to. The check runs
  after selection, so there is no way to steer a pick into an unlisted target.
- **Usage records and logs.** The record's `provider` and `model` are the target's. A new additive
  field `deployment` (the name, or `null`) lets a consumer group by what the caller addressed. The
  access log's model field stays what the caller sent.
- **Reload.** Deployments reload with the providers and routes (0022): a reload builds a whole new
  router (the balancing state starts fresh) and an admitted request keeps the snapshot it was
  resolved on.
- **No table, no change.** Nothing in a file without `[deployments]` differs, byte for byte.

## Failover (a second slice, designed here so the first does not preclude it)

Selection spreads load; it does not rescue a request whose target fails mid-call. The rule for a later
slice, stated now so slice 2's shape allows it:

- Only a **retryable failure before any response byte reached the caller** is retried on another
  target: 5xx, transport failure, and a 429 whose own backoff exceeds the request's remaining deadline.
  Never after streaming began, never a 4xx from the provider (the request is wrong everywhere), never a
  local refusal (a budget or rate limit of the **caller's** is not the target's fault).
- The retry is a **new submission** to the next target's lane with a fresh admission, so it reserves its
  own spend and rate tokens (the rule that retries, repairs and fallbacks each reserve, 0010) and writes
  its own usage record (every attempt already does), all under the one request id.
- At most `min(targets, 3)` attempts, each target at most once, bounded by the request's deadline.
- Idempotency (the `Idempotency-Key` cache) keys on the request, not the target, so a retried call
  is one logical request.

## What it does not do

- **No cross-replica balancing.** Each replica keeps its own counters; with equal weights the fleet
  averages out, with skewed replica counts it approximates. A shared counter would add a Redis round
  trip to every request for a statistical nicety; if a deployment needs exact shares it is its own
  slice.
- **No latency-aware or cost-aware choice**, no sticky sessions. Prompt caching (0003) is per account, so
  a conversation that moves between accounts loses its cache; a stable choice per conversation
  (affinity by a caller-supplied header) is the likely first extension and is deliberately not decided
  here.
- **No mixing of different models as one name**: targets are expected to be interchangeable. The file
  cannot prove it; a deployment whose targets differ in capability (tools, context length, reasoning
  controls) gets whichever the pick lands on, and the failure is the target's own 400. The reference
  says so.
- No per-target settings, headers or credentials in the deployment: a credential is a provider entry.

## Contract changes (each with its own test when built)

- **`[deployments.<name>]`** (`strategy`, `targets[]` with `model` and optional `weight`), validated by
  path; the JSON Schema changes and `serve-config.schema.json` is regenerated; the table reloads.
- **`deployment/<name>`** as a model string, resolved in the router (a new `Router` registration;
  `Route` and the other public items in the semver-listed files are unchanged, the router change is
  additive).
- **`allowed_models`** (0028) learns `deployment/<name>` and `deployment/*`.
- **Usage record field `deployment`** (additive, 0010).
- **0014** gains rows: a target whose circuit is open is skipped, all open falls back to the single-target
  rule; a reload swaps the table and an in-flight request finishes on its snapshot; an allowlist that
  lists a target but not the deployment name.
- **Metrics**: `llmshim_gateway_deployment_selected_total{deployment,target}`; the label sets are the
  configured names, so their cardinality is bounded by the file.

## Tests (when built)

- Selection: over `sum(weights)` requests each target is picked `weight` times and interleaved (a
  unit test on the selector with an injected clock-free counter); `round_robin` cycles; `ordered` takes
  the first healthy; an open circuit is skipped and its share goes to the others; all-open ignores health.
- Process: two mock upstreams as two providers; a stream of requests addressed to the deployment reaches
  both in the configured ratio; each request is rate limited, queued and budgeted as the target it picked
  (per-provider limits are different per target); the allowlist cases above; the usage record names the
  deployment and the target; a reload that removes a target stops sending to it.
- Validation by path for every refusal listed.
- Mutation checks: skipping the breaker check, and matching the allowlist against the deployment name
  instead of the pick, each fail a test.

## Slices

1. This note.
2. The file table, the router resolution (selection only), the allowlist rule, the usage field and the
   metric, with the tests above.
3. Failover before the first byte, in the gateway's request handlers.
4. Status (a deployment's targets and their circuit state in `/v1/gateway/stats`) and the 0014 rows
   for failover.

## Not decided here

- Cross-replica balancing and latency-aware selection (above).
- Affinity for prompt caching.
- Whether a deployment may carry settings of its own (today routes carry them and do not chain, so a
  default-settings deployment would be two names for one thing).
