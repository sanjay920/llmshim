# 0030: Tenant budgets above key budgets

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a
file table, an admission rule, a refusal reason and ledger keys become promises, and it touches the
spend ledger (0010), which every guarantee in 0027 rests on. It is the third slice of the roadmap
item "hierarchical budgets, tags, alerts, cost exports" (after 0028 and 0029). Nothing here is
started.

## What the ledger does today

Read from `src/gateway/attempt.rs` and `budget.rs`:

- A budget is a property of a **key** (`keys[].budget_usd`, `budget_window_secs`,
  `budget_allow_unpriced`), but the **counter is the tenant's**: its Redis key is
  `llmshim:gw:budget:v1:{sha256(tenant)}:{window_secs}:{window_index}`, and the local ledger keys a
  window the same way. Every key of one tenant that shares a window length adds to one counter.
- Each key applies **its own limit** to that shared counter: a request is refused when
  `counter + reservation > this key's limit`. Two keys of one tenant with $10 and $100 therefore share
  a total and see different ceilings. The file does not require such keys to agree (it requires only
  that `rpm` and `tpm` agree, `validate_tenant_limits`), and the code comment says they should.
- A key with **no** `budget_usd` does not touch the counter at all (its scope carries no budget).
  Two keys with different `budget_window_secs` do not share a counter: the window length is part of
  the counter's name.
- That is already a two-level idea (a shared total, a per-key ceiling), but it cannot say "this
  tenant may spend $1000 a day in all, and this one key at most $50 of it": a key's own spend is not
  counted separately, so a $50 key cap would be consumed by its capped siblings, and uncapped siblings
  are not limited by the total at all.

## The rule

Two levels, each with its own counter, and a request must fit **both**:

```toml
[tenants.payments]            # a new optional table, one per tenant name used by [[keys]]
budget_usd = 1000.0
budget_window_secs = 86400
budget_allow_unpriced = false

[[keys]]
name = "checkout-prod"
tenant = "payments"
budget_usd = 50.0             # this key's own cap, its own counter
```

- **Without a `[tenants.<name>]` table nothing changes**: the tenant keeps the shared counter and the
  per-key ceilings described above, byte for byte (same Redis keys, same refusals). This is the
  compatibility rule the whole note is built on.
- **With the table, the tenant level is a counter of its own in a new namespace**:
  `llmshim:gw:budget:v1:tenant-cap:{sha256(tenant)}:{window_secs}:{window_index}`, limited by the
  table's `budget_usd`. A key's `budget_usd` then caps **that key's own spend** on a counter named by
  the **key** (not its secret, so a rotation keeps its cap):
  `llmshim:gw:budget:v1:key-cap:{sha256("{tenant}/{key name}") prefix}:{window_secs}:{window_index}`.
  A key with no `budget_usd` has only the tenant cap, and **every key of the tenant, capped or
  not, counts toward and is limited by the tenant cap**. The pre-table shared counter is no longer
  used for that tenant and expires on its own.
- **Introducing a table is not silent.** Because both new counters are new keys, they **start at zero
  in the window in force**: what the tenant's keys spent earlier in that window is counted at neither
  level, and keys that had no `budget_usd` (previously unlimited) become limited by the tenant cap from
  that moment. An operator adds a table at a window boundary to avoid a one-window over-allowance, and
  the reload status reports each table's `started_at`. Changing a table's `budget_window_secs` starts a
  fresh counter the same way. This is documented in the file's reference and in the reload answer, and
  it is the price of never reinterpreting an existing counter.
- **A key's cap needs the key's identity at admission.** The policy scope (which crosses Redis to the
  replica that dispatches a job) gains the key-cap descriptor (counter id, limit, window, unpriced flag)
  and the tenant-cap descriptor, and its `version` becomes 2 **only for a request that has either**;
  every other request keeps version 1 and is byte-for-byte today's. A replica that predates the change
  refuses a version 2 scope (`is_current()`), so a job is never run with a level silently unenforced:
  upgrade every replica before adding the first table.
- **A request is admitted only if it fits every level it has**, and the reservation is taken at every
  level or at none: both counters move in the same atomic step (the same Lua script call in a fleet,
  the same lock in the in-memory ledger), so a refusal at one level never leaves a reservation at the
  other. Every level is evaluated before the answer, so when more than one refuses the `Retry-After`
  is the latest of their window ends; a refusal for retention or a global freeze (code 4) still
  outranks any budget refusal. Settlement adjusts each level **by its own liability**: an unpriced
  attempt holds `limit - current` at each level, and those differ, so the attempt record carries a
  liability, a counter key and a freeze key **per level** (below).
- **The refusal** is the existing `429 attempt budget exhausted` with the existing
  `Retry-After` (the end of the window of the level that refused; the later of the two when both
  refuse). A new `level` (`"tenant"` or `"key"`) is added to the refusal's `metadata`, to the
  `llmshim_gateway_rejected_total` reason label (`budget_tenant`, `budget_key`) and to the access
  log's refusal line; the message text does not change and names no amount.
- **Windows** are independent: the tenant table has its own `budget_window_secs` and a key its own;
  the two may differ (a daily tenant cap over a monthly key cap is allowed and meaningful). Each counter
  keeps **its own TTL** (its window's end plus the retention), the attempt record lives as long as the
  longest of them, and settlement **never resurrects an expired counter**: it checks that the counter
  exists before adjusting it and skips one that is gone (its window ended; there is nothing to
  correct). The single-counter path keeps its present script unchanged.
- **Unpriceable attempts.** Today an unpriceable attempt is refused or, with `budget_allow_unpriced`,
  holds the whole remaining allowance until settled (0010). With two levels it holds the remaining
  allowance **at each level that allows it**, and is refused if **any** level does not allow it: the
  tenant table's `budget_allow_unpriced` and the key's flag must both be true for an unpriced
  attempt to be admitted when both levels exist. A level with no budget does not vote.
- **Strictness** is unchanged: a level with a budget needs trusted pricing and a token bound for a
  finite reservation, as today.
- **Validation (by path).** `tenants.<name>` must be a tenant used by at least one key (a typo would
  otherwise be a silent no-op); `budget_usd` finite, at least 0 and at most the ledger's exact maximum
  (the same rules as a key's); `budget_window_secs` within the existing bounds; a table with
  `budget_window_secs` or `budget_allow_unpriced` and no `budget_usd` is refused, as for a key. A
  tenant table does **not** change the `rpm`/`tpm` rule.
- **Reload.** The table reloads with the keys (0012): the next request is judged against the new
  limit; counters keep what they hold; a newly added or re-windowed table starts fresh counters as
  described above.

## What it does not do

- No third level (project, team) in the file: `serve` has tenants and keys. The control plane's
  `tenant` is a key's team, so a team's budget maps onto the tenant level (slice 5: an optional budget
  on the snapshot's tenant record, **emitted only when set**, so a snapshot without one is the one
  older data planes read; upgrade data planes first); a project has no home yet because a key carries
  only its team to the data plane. The design is a **list of counters per request** (at most three)
  so a middle level adds no new admission code.
- No budgets on tags (0029): a tag is a label. A cap per cost center would be a level, and it needs
  the tenant/team levels decided first.
- No alerts or notifications when a level nears its cap (a separate roadmap item; it needs a
  delivery channel and so its own note). `/v1/gateway/stats` gains each level's remaining headroom
  only if a later slice decides it is safe to expose.
- No borrowing between tenants, and no rollover of an unused window.

## Interaction with outage admission (0027)

A lease is a reservation inside one counter, and the lease code takes the counter's name as a
parameter, so a lease on the tenant-cap counter works as today. **A key with its own cap is refused
in outage state in the first build.** This is a cost, not a safety necessity: the local outage view
could carry a second local counter with its own lease on the key-cap counter (the lease machinery
is generic over the counter prefix), at the price of registering key counters on the healthy path
and the same multi-counter change to the local ledger. It is deferred because, with a tenant table,
the likeliest deployment caps every key, so the refusal removes most of 0027's value for those
tenants; slice 4 either builds the second lease or says plainly that 0027 and key caps are exclusive.
Keys with no cap of their own under a tenant table are admitted in outage state as under 0027, and
the lease size is then set by the table, which removes today's last-writer-wins on `register()`.

## Contract changes (each with its own test when built)

- **`Identity`** carries the key's name (for the key-cap counter), and the scope's version rule above.
- **`[tenants.<name>]`** with `budget_usd`, `budget_window_secs`, `budget_allow_unpriced`, validated by
  path; the JSON Schema changes and `serve-config.schema.json` is regenerated.
- **Ledger keys**: the per-key counter (`...:key:{credential_prefix}:...`), its attempt records and
  index entries, in the same retention and tombstone accounting as today's (they count against
  `max_retained_attempts`; a request with two levels holds **one** attempt record that lists both
  counters, so the cap counts attempts, not counters).
- **The attempt record and the scripts.** The record keeps its present fields for the first
  counter (`liability`, `total_key`, `freeze_key`, `bounded`, ...) and a second and third level add
  suffixed fields (`liability_1`, `total_key_1`, `freeze_key_1`, ...). A request with one counter
  writes exactly today's record and runs today's scripts; a request with more runs a script that
  loops over the levels (the script text changes, its single-level layout does not). The in-memory
  ledger's `AttemptLiability` gets the same per-level list. `bounded` is per attempt, not per level.
  The legacy spend floor stays tenant-only and applies only to a tenant with no table.
- **Refusal metadata and metric labels** as above.
- **0010** (usage records): no new field; `cost_usd` per attempt is unchanged. A record's tenant and
  key already identify both levels.
- **0014** gains the rows: the tenant cap refuses a key under its own cap; a key cap refuses while the
  tenant has room; both counters move or neither (a refusal leaves both unchanged); an unpriced attempt
  needs both flags; a reload that adds a table; a key with its own cap is refused in outage state.

## Tests (when built)

- Process, in-memory and fleet: a tenant cap shared by two keys (the second key's spend counts); a key
  cap that refuses while the tenant has room and the reverse; the unchanged behavior without a table
  (the same refusal at the same point as before, pinned against today's tests); `Retry-After` is the
  refusing level's window end.
- Atomicity: with Redis, a request refused at the key level leaves the tenant counter exactly as it
  was, under concurrency (many requests racing both counters never leave the tenant counter above its
  cap or a reservation orphaned); settlement over both counters; a crash between reserve and settle
  leaves both liabilities (0014's settlement row, once per level).
- Mutation checks: removing the tenant level from admission, and settling only one level, each fail a
  test.
- A late settlement after the shorter window's counter expired creates no key (the longer counter is
  still corrected); an unpriced attempt that finishes cheaply returns each level's own liability; a
  rotated key keeps its cap; a version 2 scope is refused by a replica that does not know it.
- Window independence: a daily tenant window over a monthly key window.
- The compatibility rule: a golden test of the Redis keys and Lua arguments for a table-less tenant
  equal to today's.

## Slices

1. This note.
2. The file table, validation, schema, and the in-memory ledger with two counters (single instance),
   with the process tests and the compatibility golden test.
3. The Redis ledger: the scripts over a list of counters, settlement, the retention accounting, the
   atomicity and concurrency tests against a real Redis.
4. Outage interplay (the refusal for a capped key), metrics labels, status, 0014 rows.
5. The control plane: a team's budget as the tenant level (its own signed-format note section).

## Not decided here

- Whether the tenant table should also carry `rpm`/`tpm` (today they are per key and must agree), which
  would remove that awkward rule; it is a separate cleanup with its own compatibility question.
- A middle level (project) and its place in the list of counters.
- Alerts when a level nears its cap.
