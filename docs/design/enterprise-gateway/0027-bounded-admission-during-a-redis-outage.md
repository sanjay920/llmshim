# 0027: Bounded admission during a Redis outage

Status: design, 2026-11-17 (revised after review). A contract note, written before any code (README
working rule 3): a `[fleet]` setting, a usage-log field and a failure row each become promises. It
is the item 0024 split out of the control-plane note; it needs the fleet (0006), the spend ledger
(0010) and the failure table (0014), and it changes none of them until it is built. Nothing here
is started.

## The problem

With `[fleet]`, every request goes through the Redis queue (0006). When Redis is unreachable the
replica refuses new work at once (0014): **safe, not available**. A deployment may prefer to keep
serving a bounded amount while Redis is out, as an operator's choice, knowing what it costs. This
note says what "bounded" can honestly mean, because the obvious statement ("overspend is bounded by
the sum of local allowances") is **wrong** for this ledger, and a design that over-claims is worse
than refusing.

## What the ledger does today, and why that rules out the obvious design

Read from `src/gateway/attempt.rs` and `budget.rs`:

- **The counter belongs to the tenant, not the key.** The ledger key is
  `llmshim:gw:budget:v1:{sha256(tenant)}:{window_secs}:{window_index}`. Several keys of one tenant
  with the same `budget_window_secs` share one counter; keys with another window length have another.
  Rate buckets are likewise per tenant and provider. So a lease is per
  **(replica, tenant, window_secs)**, never per key.
- **A reservation is per attempt.** One Lua call adds the attempt's quote to the window total and
  writes a per-attempt hash; settlement (`SETTLE_BUDGET_LUA`) corrects the total once, returns an
  error if the hash is absent, and is a no-op on a second call. It cannot draw down a larger
  reservation in steps, and an attempt admitted locally has no hash. **New Lua is required.**
- **A reservation is not released by expiry.** Its amount sits in the window counter until the
  counter expires (window end plus retention); only an explicit settle returns money earlier.
- **The window index comes from Redis `TIME` when the attempt is admitted.** A late settlement posts
  to the window of admission.
- An unpriceable attempt either is refused or, with `budget_allow_unpriced`, holds the whole
  remaining allowance. Settlement can exceed a reservation, and a partial provider bill is a lower
  bound.
- A fleet request needs the queue, the shared rate buckets and the shared concurrency accounting, and
  the gateway has one backend and one attempt coordinator, both Redis-backed in fleet mode. An outage
  mode is therefore **a second admission path**, not a spend rule.

What can be bounded is **admitted reservations**, not spend, and only for attempts whose cost is
bounded before they are sent.

## The design

Off by default. With `[fleet.outage_admission]`, a replica may, **while it is in outage state**, admit
requests locally under limits fixed in advance, and **never otherwise**:

```toml
[fleet]
redis_url = { env = "REDIS_URL" }
[fleet.outage_admission]
spend_lease_fraction = 0.05   # target share of a tenant's remaining window held as this replica's lease
rate_fraction = 0.10          # of each provider's and each tenant's rpm/tpm limits, per replica
max_concurrency = 16          # in-flight upstream requests per replica during the outage
recover_after_secs = 30       # Redis must answer for this long before the replica leaves outage state
```

1. **Outage state, with hysteresis.** A replica enters it when its Redis operations fail or exceed
   their deadline repeatedly (a count within a short window, as the breaker counts); it leaves it
   only after Redis has answered every probe for `recover_after_secs` **and** recovery (below) has
   finished. A flapping or slow Redis therefore keeps the replica in the outage path rather than
   switching per request. Detection is the shared health code's; the thresholds are fixed, not
   configurable, in the first build.
2. **Who is admitted.** A request whose own key has a budget (`budget_usd`) **and** a lease for its
   (tenant, window) with room, and whose attempt can be priced and bounded (`max_tokens` known, model
   priced). A key without a budget is refused even when a sibling key of its tenant has one: the
   budget is a property of the key's identity and the lease is a property of the tenant's counter,
   and the unbudgeted key has nothing to be bounded by. An unpriceable attempt and
   `budget_allow_unpriced` are refused with the outage reason.
3. **A spend lease, taken while Redis is healthy.** For each (tenant, window) a replica serves
   budgeted keys of, it **tops its lease up to a target**, never adds a fraction on every tick:
   `target = spend_lease_fraction × (limit − counter + own lease)` read atomically, and the top-up is
   `target − own lease` when positive. The lease is a reservation in the tenant's window counter
   recorded in a **lease hash** (owner replica id, tenant, window, amount, deadline = window end) and
   indexed. Renewal runs every 10 s and on first use. A lease is only as large as the target and is
   returned (shrunk) when the target falls.
4. **Window boundary.** A lease belongs to the window it was taken in. Spending it in the next
   window would be uncovered by that window's counter, so the replica **stops admitting on a lease
   within a fixed margin (30 s) of its window end** by its own clock and refuses until Redis returns
   and a new lease is taken. A long outage straddling a boundary therefore admits nothing past it:
   stated, not hidden. The margin covers clock skew between the replica (system time) and Redis
   (`TIME`, which picks the window); a skew larger than the margin is a configuration fault and the
   0014 "wall-clock changes" gap, not something this mode fixes. Settlement after recovery posts to
   **the lease's window**.
5. **Local limits.** Rate buckets for `rate_fraction` of each configured limit, seeded fresh at the
   start of the outage; `max_concurrency` in flight. The local path has **its own** scheduler,
   limiter, concurrency cap and attempt coordinator (`LocalAttemptRates`, whose budget is seeded from
   the lease instead of `budget_limit`), and the gateway state holds both paths and chooses by outage
   state; `ready()` and the stats read the chosen one. While active, the local concurrency cap
   **replaces** the fleet's backpressure semaphores. The local lane's queue depth is **zero**: a
   request that cannot run now gets 503 with `Retry-After`; the mode is not a queue.
6. **Idempotency and receipts.** Scoped idempotency lives in Redis today and is not available during
   the outage; the local path keeps an **in-memory** cache per replica. A client retry that reaches
   another replica can therefore be billed twice. This is a stated cost of the mode. The native
   facades' replay receipts are local files and keep working.
7. **Recovery**, in order, and **resumable**:
   1. stop admitting locally (the replica stays in outage state);
   2. wait for in-flight local attempts to end, with a bound; one that does not end in time is
      settled as unfinished at its reservation, as a fleet attempt that times out is;
   3. **draw down**: for each locally admitted attempt, one idempotent call (new Lua, keyed by an
      attempt id kept in a per-lease record set) charges the final amount to the lease and, when it
      exceeds what the lease held, charges the excess to the window counter and records it like a
      fleet attempt's excess;
   4. **release** what is left of the lease (shrink the counter by the remainder, delete the lease);
   5. only then, and after `recover_after_secs`, return to fleet admission.
   If Redis drops again mid-recovery, nothing is lost or counted twice: the draw is idempotent per
   attempt id, so the next recovery resumes where this one stopped, and the replica stays in outage
   state until step 4 completes.
8. **A dead replica's lease is stranded until its window ends.** The money stays in the counter
   because nothing returns it earlier; there is no early reclaim in the first build (a reaper that
   returned a lease on a stale heartbeat could take it from a live replica that cannot heartbeat
   during an outage). The cost is stated: fraction × tenant budget, per dead replica, for at most one
   window. A restarted replica has no memory of its lease, so it refuses until Redis returns; the
   Redis half of its old lease is stranded the same way.

### The bound, stated exactly

At any moment during an outage, the amount admitted locally for a (tenant, window) is at most the
sum over replicas of **(lease taken before the outage − the quotes of that replica's local attempts)**:
each local attempt, **including each retry, repair and fallback attempt**, reserves its own quote
from the lease before it is sent, in process memory, and a quote that exceeds the unspent lease is
refused. This is a bound on **quotes**. Final spend can exceed a quote by what the provider bills
above it (the slack a fleet attempt has), and an attempt that cannot be priced is never admitted.
In-flight work from before the outage (a job already in the Redis queue or leased, 0006) is the
fleet's, not this mode's: it fails as today.

### The cost in good times

Each replica holds up to its target, so N replicas holding fraction f of what remains leave the fleet
`(1 − f)^N` of a tenant's budget: 20 replicas at 0.05 lock about 64 % of it and the rest is what fleet
admission sees; at 0.05 and 50 replicas, 92 %. A deployment sizes `spend_lease_fraction` as roughly
`(1 − (1 − reserve)^(1/N))` for the share `reserve` of budget it is willing to hold back, and the
file's check warns when it cannot know N. This price is paid continuously, whether or not an outage
ever happens, which is why the mode is off by default.

## What it does not do

- It does **not** coordinate rate buckets across replicas: the fleet-wide rate during an outage is at
  most `replicas × rate_fraction` of the limit, and `rate_fraction` is the operator's estimate of
  `1 / replicas`. It does not track autoscaling; a deployment that scales widely sets it for the
  largest size, or leaves the mode off. Locally admitted requests are **not debited** from the shared
  buckets afterwards, so for a short time after recovery a replica's usage can exceed the limit; 429
  penalties learned during the outage are not shared.
- It admits a streaming request only when its cost is bounded before it begins (`max_tokens` known,
  model priced), the same test as a unary request; a stream without that bound is refused.
- It does not survive a replica restart during the outage (see 8).
- It does not make the replica healthy: `/health` is unchanged and the fleet's work is still refused.

## `/ready`

The Helm chart's readiness probe uses `/ready` (0008), so a `/ready` that stays 503 would take every
replica out of the Service in the outage and the mode would serve nothing. With the mode on,
`/ready` therefore **stays 200 while outage admission is active**, and its body reports
`"admission": "outage"` and the unspent lease; it is 503 only when the replica can serve by neither
path (no lease, or all margins passed). A deployment that wants Kubernetes to drain on a Redis
outage leaves the mode off. The startup probe still uses `/health`.

## Contract changes (each with its own test when built)

- **`[fleet.outage_admission]`** as above, validated by the file's rules (fractions in (0, 1],
  `max_concurrency` 1 to 4096, `recover_after_secs` 5 to 3600), refused without `[fleet]`; the JSON
  Schema changes and `docs/design/enterprise-gateway/serve-config.schema.json` is regenerated (compared
  by `tests/unit_serve_cli.rs`); `example.toml` gains the table.
- **A usage-log field** `admission`: `"fleet"` for every attempt today and `"outage"` for an attempt
  admitted on a lease. Additive; it is defined in 0010 (which is updated), and joins the closed
  attribute set of 0016's spans when telemetry is on.
- **Redis keyspace**: lease hashes and their index, and the per-lease draw record set, each with a TTL
  to window end plus the ledger's retention. They are **separate from the attempt tombstones**, so
  they neither count against nor can freeze the tombstone cap (0010); the catalog of keys in 0010 gains
  them.
- **Status and metrics**: `/v1/gateway/stats` and the admin listener report `outage_admission_active`,
  the unspent lease per (tenant, window) as a hash, never a name, and the recovery state; `/metrics`
  gains the matching series.
- **0006**'s "Redis outage" operating note (which says fail closed and names this as a roadmap item)
  and **0024**'s pointer to this note are updated when the mode ships.
- **0014** gains the rows: Redis lost with the mode on and a budgeted key (admitted up to the lease,
  then refused); an unbudgeted key (refused); a window boundary crossed mid-outage (refused past the
  margin); recovery (drawn, released); recovery interrupted by a second outage (resumed, no double
  charge); a flapping Redis (stays in outage state); a replica killed during the outage (lease
  stranded to window end). Until the mode is built, "outage admission" joins the "not yet tested" list.

## Tests (when built)

- Process, with the TCP-proxy outage harness of `integration_serve_outage`: with Redis cut, a budgeted
  key is served until its lease is spent and then refused, an unbudgeted key (also when a sibling key
  has a budget) and an unpriceable attempt are refused at once; the provider mock is hit exactly the
  admitted number of times.
- The bound itself: a property-style test spends past the lease and asserts that the sum of quotes
  never exceeds it, and that after recovery the ledger shows the drawn amount and the released
  remainder.
- Recovery is idempotent: kill the replica's Redis connection after k draws, resume, and assert the
  ledger equals the uninterrupted result.
- The window margin: a lease near its window end admits nothing.
- Top-up never compounds: many renewals hold the lease at the target.
- A replica killed mid-outage strands its lease until window end and no more.
- The mode off (the default) is byte-for-byte today's behaviour: the existing outage tests pass
  unchanged.

## Slices

1. This note.
2. **Built** (`src/gateway/lease.rs`, `gateway-redis` only, unused outside its tests): the Lua (take or
   top up, draw idempotently, release) and the lease on the healthy path, with no admission change: tested against a real Redis for the bound, the window, idempotent draw and
   stranding.
3. **Built** (`LeaseLedger::recover`, still unused outside its tests): recovery: the draw-down and release procedure over synthetic local records, resumable. It lands
   **before** any local admission, because admitting without a way to settle is the unsafe order.
4. The outage state with hysteresis, the second scheduler, limiter and coordinator, the local
   admission path, the `[fleet.outage_admission]` table and the idempotency cache.
5. Status, metrics, `/ready`, the usage field and the 0014 rows.

## Not decided here

- Whether a deployment with a Redis cluster or sentinel failover needs this at all: those make the
  outage shorter, not impossible, and the note does not assume either way.
- Hierarchical budgets (a team or project above the tenant), which wait for the budget note; this note
  leases against the tenant's counter as the ledger has it.
- Early reclaim of a dead replica's lease.
