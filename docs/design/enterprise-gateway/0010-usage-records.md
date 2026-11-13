# 0010: Usage and cost records per tenant

Status: implemented. Closes the gap that 0009 left open.

## What it is

```toml
[logging]
usage_log = "stdout"        # or an absolute file path, like access_log
```

One JSON line per provider **attempt**, when the attempt ends:

```json
{"event":"usage","ts":"2026-11-13T10:15:30.870Z","attempt_id":"6f1c…","tenant":"payments",
 "provider":"openai","model":"gpt-6-luna","kind":"completion","outcome":"completed",
 "http_status":null,"duration_ms":812,"input_tokens":1000,"output_tokens":500,
 "reasoning_tokens":null,"uncached_input_tokens":1000,"cache_read_tokens":0,
 "cache_write_tokens":0,"cost_usd":0.00035,"cost_source":"catalog","usage_reported":true}
```

`outcome` is `completed`, `http_failure` (with `http_status`),
`transport_failure`, `invalid_response`, `stream_failure` or `abandoned`.

Token fields, for chargeback:

- `input_tokens` is the provider's own prompt count, and providers disagree on
  whether it already contains cache reads (Anthropic's does not; OpenAI's and
  Gemini's do). Do not add `cache_read_tokens` to it blindly. Use
  `uncached_input_tokens` (the part billed at the full input rate) with
  `cache_read_tokens` and `cache_write_tokens`.
- `reasoning_tokens` and `uncached_input_tokens` are `null` when the provider
  reported no such count, which is not the same as zero. Chat-wire providers
  report reasoning inside `completion_tokens_details`; both places are read.
- **Known undercount, shared with the spend ledger:** on the Gemini native path
  the gateway's normalized `completion_tokens` is Google's
  `candidatesTokenCount`, which excludes thinking tokens (`thoughtsTokenCount`),
  while Google bills them as output. The record, the cost and the budget cap all
  use that figure, so a Gemini thinking model is under-counted in all three
  until the shared normalization folds the thoughts in, as the non-streaming
  response already does. It is a separate fix to shared accounting, outside
  this log.

## Per attempt, not per request

The gateway already meters every provider send to enforce spend caps, and the
unit of that metering is the attempt. A retry, a schema repair and a fallback
are each a separate send, so each is a separate record. Summing `cost_usd` per
tenant gives what the tenant caused the gateway to spend, including the part
that failed over. A request's attempts share a tenant and are close in time;
they are not yet joined by request id (see below).

`cost_usd` follows the gateway's accounting rule everywhere: **`null` is
unknown, never free**. A failed attempt that reported no usage has a `null`
cost and `usage_reported: false`. A sum that must be exact treats a null as a gap
to chase, not as zero. `cost_source` says how the figure was made: `provider`
(an exact terminal bill), `provider_floor` (the highest partial bill seen, a
lower bound) or `catalog` (an estimate from published prices).

**A record settles exactly as the spend ledger does**, so the two cannot
disagree about what an attempt cost. A provider's exact terminal bill wins.
Otherwise the terminal catalog estimate is used, but never below the highest
charge the provider reported along the way: a stream that reported 0.08 partway
and then ended with a catalog estimate of 0.015 is recorded as 0.08
(`provider_floor`), the amount the cap counted. An attempt that did not complete
(failed or abandoned) records what is known as a lower bound: the highest
provider charge as `provider_floor`, or the latest snapshot as it was stamped.
A test replays the ledger's own sequence (80, 80, then 15) against the record.

## How it is built

- `gateway::usage` defines `UsageRecord`, the `UsageSink` trait and the recorder.
  The coordinator that already meters attempts gets an optional recorder.
  Without a sink nothing changes.
- An attempt is **admitted** (the rate, tenant and budget checks passed and the
  send is about to happen), then its latest cumulative usage snapshot is kept,
  and when it finishes or is abandoned exactly one record is emitted. A
  refused attempt was never sent, costs nothing and produces no record. A
  finish followed by a drop notification produces one record, not two. At most
  the latest snapshot of each attempt in flight is held.
- **The tenant's name does not travel with the job.** The policy scope that
  crosses Redis in a fleet carries only a SHA-256 of the tenant, and the
  gateway's tests assert that a tenant's name never appears in a job descriptor:
  names are kept out of shared storage on purpose. Usage records therefore get
  the name locally. Each replica hashes the tenants in its own configuration
  and looks the scope's hash up. Replicas of one fleet share a configuration, so
  they agree, and nothing in the job format changes, so a fleet upgrades in
  place with no compatibility window. A tenant a replica's configuration does
  not name (a key removed from this replica's file during a rollout) is recorded
  with `tenant: null`. A server running with `allow_unauthenticated` attributes
  every request to the tenant `anonymous`, which no key names, so that tenant is
  added to the names a replica knows.
- `serve` writes the records with the same JSON-lines writer as the access log.
  Both carry an `event` field (`access`, `usage`) so they can share `stdout`,
  and two logs cannot name the same file (a configuration error).
- **The writer never blocks the gateway.** Records are queued (8192 of them) for
  a thread that does the writing, so a stalled pipe or a slow disk cannot hold a
  request, a concurrency permit or a lock inside the gateway. When the queue is
  full new lines are dropped and counted, the first drop is reported on stderr,
  and the totals are reported at shutdown; a write that fails is counted the
  same way. Each line is one `write_all`, so a failure cannot leave half a line
  for the next to be glued to. Shutdown writes what is queued before exiting.

An attempt that was admitted and then dropped before its request left (a
deadline expired, the caller disconnected) is recorded as `abandoned` with
`usage_reported: false`: it consumed an admission, and a count of these is a
useful signal. A request refused before admission leaves no record.

## What is proven

Unit tests: one record per admitted attempt with the final snapshot of tokens
and cost; an attempt with no usage has a null cost; finish-then-drop is one
record and a bare drop is `abandoned`; a refused attempt leaves none; a tenant
the replica does not know is `null`; and the policy scope never carries a
tenant's name, only its hash.

End to end, against a real server: a priced model (GPT-6 Luna, 1000 input and 500
output tokens at $0.10 and $0.50 per million) yields `cost_usd` 0.00035 from the
catalog, attributed to the key's tenant, with no secret, prompt or completion in
the file; an upstream that always fails yields one `http_failure` record per
send (a retry is two), each with a null cost; and in fleet mode, with every
request entering replica A, both replicas dispatch work and **every record names
the tenant**, including those written by replica B.

## Not done

- **Joining a request's attempts by request id.** The scope is built from the
  caller's identity; carrying a request id would extend the job format. It is a
  small follow-up if grouping the attempts of one request matters more than
  per-tenant totals.
- **The API key's name** (not just the tenant) on a record, for per-key
  chargeback. The same local resolution would work, keyed by the credential's
  digest, if the scope carried it.
- **Delivery guarantees.** A record is queued and written best effort. A crash
  between the provider's response and the write loses it, a closed pipe or a
  full queue drops it (counted, not silent). Exact billing reconciliation should use the provider's invoice; this
  attributes it. A durable, exportable sink (Kafka, object storage) is a later
  item.
- **Streams still open at shutdown** are recorded as `abandoned` with the usage
  seen so far, which is a lower bound.
