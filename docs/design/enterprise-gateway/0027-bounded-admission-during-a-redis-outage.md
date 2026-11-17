# 0027: Bounded admission during a Redis outage

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a
`[fleet]` setting, a usage-log field and a failure row each become promises. It is the item 0024
split out of the control-plane note; it needs the fleet (0006), the spend ledger (0010) and the
failure table (0014), and it changes none of them until it is built. Nothing here is started.

## The problem

With `[fleet]`, every request goes through the Redis queue (0006). When Redis is unreachable the
replica refuses new work at once (0014): **safe, not available**. A deployment may prefer to keep
serving a bounded amount while Redis is out, as an operator's choice, knowing what it costs. This
note says what "bounded" can honestly mean, because the obvious statement ("overspend is bounded by
the sum of local allowances") is **wrong** for this ledger, and a design that over-claims is worse
than refusing.

## What the ledger does today, and why that rules out the obvious design

- Every attempt is admitted through a **reservation**: configured `budget_usd` is reserved before
  each send, including retries and fallbacks (0010, `gateway/attempt.rs`, `budget.rs`), in a Redis
  window shared by the fleet. An attempt whose cost cannot be priced either is refused or, with
  `budget_allow_unpriced`, **holds the whole remaining allowance** until settlement.
- Settlement can exceed the reservation (the provider reports more than was bounded), and a partial
  provider bill is a lower bound, not a total.
- A fleet request cannot be admitted at all without the queue, the shared rate buckets and the shared
  concurrency accounting. So an outage mode is **a second admission path**, not a spend rule.

Therefore what can be bounded is **admitted reservations**, not spend, and only for requests whose
cost is bounded before they are sent.

## The design

Off by default. When `[fleet] outage_admission` is set, each replica may, **while it cannot reach
Redis**, admit requests locally under three limits that are all fixed in advance, and **never
otherwise**:

```toml
[fleet]
redis_url = { env = "REDIS_URL" }
[fleet.outage_admission]
spend_lease_fraction = 0.05   # of a budgeted key's remaining window, taken while Redis is healthy
rate_fraction = 0.10          # of each provider's and each key's rpm/tpm limits, per replica
max_concurrency = 16          # in-flight upstream requests per replica during the outage
```

1. **Who is admitted.** Only a request from a credential **with a budget** (`budget_usd`) and whose
   attempt can be **priced and bounded** (`max_tokens` known and the model priced); an unbudgeted
   key, an unpriceable attempt and `budget_allow_unpriced` are refused as they are today, with the
   outage reason. A budget is what gives the lease something to be a fraction of; without one there is
   no bound to offer.
2. **A spend lease, taken while Redis is healthy.** Each replica, periodically (default every 10 s)
   and on first use, **reserves** `spend_lease_fraction` of the key's remaining window from the shared
   ledger **as a reservation owned by the replica** (the same record kind, tagged with the replica's
   id and an expiry of one window). That money is then unavailable to the fleet: this is the price of
   the mode, paid in advance, in good times. During an outage the replica admits only against its own
   unspent lease.
3. **Local rate and concurrency limits**: `rate_fraction` of each configured limit (a replica cannot
   know its peers' use, so it takes a fixed share), an in-process queue with the existing local
   scheduler, and `max_concurrency` in flight.
4. **On recovery**, in order: the replica stops admitting locally; waits for its in-flight local
   attempts; **settles** each against the ledger as an ordinary attempt (the admitted amount is
   already reserved by the lease, so settlement draws it down, and a settlement above the reservation is
   charged and the excess is recorded exactly as for a fleet attempt); then releases what is left of the
   lease. A lease whose owner died is released by its expiry (one window), **stranding** that fraction
   until then: a stated cost, which is why the fraction is small and the mode off by default.

The bound this gives, stated exactly: **at any moment during an outage the amount admitted locally,
fleet-wide, is at most the sum of the unspent leases the replicas hold (plus nothing else)**, each of
which was reserved against the key's budget before the outage began. Final spend can exceed it by what
the provider bills above a reservation (the same slack a fleet attempt has), and an attempt that
cannot be priced is never admitted in this mode.

## What it does not do

- It does **not** coordinate tenants' rate buckets across replicas (it takes a fixed share), so the
  fleet-wide rate during an outage is at most `replicas × rate_fraction` of the limit, which can be
  below or, with few replicas and a large fraction, above 100 %: `rate_fraction` is documented as the
  operator's estimate of `1 / replicas`, and a deployment that needs a hard fleet-wide rate sets it
  accordingly or leaves the mode off.
- It does not admit streaming requests whose cost cannot be bounded before they begin (a stream's
  `max_tokens` bound is its bound).
- It does not survive a replica restart during the outage (the lease is in memory and in Redis, the
  latter unreachable): a restarted replica has no lease and refuses until Redis returns.
- It is not a queue: during the outage a local request that cannot run now is refused with 503
  `Retry-After`, as in the in-process scheduler's overload response.

## Contract changes (each with its own test when built)

- **`[fleet.outage_admission]`** as above, validated by the file's rules (fractions in (0, 1],
  `max_concurrency` 1 to 4096), refused without `[fleet]`; the JSON Schema changes.
- **A usage-log field** `admission`: `"fleet"` for every attempt today and `"outage"` for an attempt
  admitted on a lease. Additive to the usage record (0010), so a consumer that ignores unknown fields is
  unaffected; the field joins the documented record and the OpenAPI/log schema in the same change.
- **Status and metrics**: `/v1/gateway/stats` and `/metrics` report `outage_admission_active` and the
  replica's unspent lease; `/ready` stays 503 during the outage (the replica is not healthy for fleet
  work) while `/health` stays 200.
- **0014** gains the rows: Redis lost with the mode on and a budgeted key (admitted up to the lease,
  then refused); with an unbudgeted key (refused); recovery (settled, lease released); a replica killed
  during the outage (lease expires, stranded).

## Tests (when built)

- Process, with the TCP-proxy outage harness of `integration_serve_outage`: with Redis cut, a budgeted key
  is served until its lease is spent and then refused, an unbudgeted key and an unpriceable attempt are
  refused at once; the provider mock is hit exactly the admitted number of times.
- The bound itself: a property-style test spends past the lease and asserts that the amount admitted
  never exceeds the lease, and that after recovery the ledger shows the settled amount and the released
  remainder.
- A replica killed mid-outage strands its lease until expiry and no more.
- The mode off (the default) is byte-for-byte today's behaviour: the existing outage tests pass unchanged.

## Slices

1. This note.
2. The lease: taking, renewing and releasing a replica-owned reservation while Redis is healthy
   (no admission change), with the stranding and expiry behaviour tested.
3. The local admission path, the `[fleet.outage_admission]` table, the status and the usage field.
4. Recovery settlement and the 0014 rows.

## Not decided here

- Whether a deployment with a Redis cluster or sentinel failover needs this at all: those make the
  outage shorter, not impossible, and the note does not assume either way.
- Per-tenant (as opposed to per-key) leases and hierarchical budgets, which wait for the budget note.
