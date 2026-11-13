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
 "reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,
 "cost_usd":0.00035,"cost_source":"catalog","usage_reported":true}
```

`outcome` is `completed`, `http_failure` (with `http_status`),
`transport_failure`, `invalid_response`, `stream_failure` or `abandoned`.

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
  with `tenant: null`.
- `serve` writes the records with the same owner-only JSON-lines writer as the
  access log. Both carry an `event` field (`access`, `usage`) so they can share
  `stdout`.

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
- **Delivery guarantees.** A record is written inline and best effort. A crash
  between the provider's response and the write loses it, and a closed pipe
  drops it. Exact billing reconciliation should use the provider's invoice; this
  attributes it. A durable, exportable sink (Kafka, object storage) is a later
  item.
- **Streams still open at shutdown** are recorded as `abandoned` with the usage
  seen so far, which is a lower bound.
