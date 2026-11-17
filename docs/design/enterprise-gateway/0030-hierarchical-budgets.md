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
- That is already a two-level idea (a shared total, a per-key ceiling), but it cannot say "this
  tenant may spend $1000 a day in all, and this one key at most $50 of it": a key's own spend is not
  counted separately, so a $50 key cap would be consumed by its siblings.

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
- **With the table, the shared counter belongs to the tenant level**: its limit is the table's
  `budget_usd`, not any key's. A key's `budget_usd` then caps **that key's own spend** on a counter
  of its own, keyed by the key's credential digest prefix instead of the tenant
  (`llmshim:gw:budget:v1:key:{credential_prefix}:{window_secs}:{window_index}`). A key with no
  `budget_usd` has only the tenant cap. The tenant's cap counts every key's spend, including keys
  with and without caps of their own.
- **A request is admitted only if it fits every level it has**, and the reservation is taken at every
  level or at none: both counters move in the same atomic step (the same Lua script call in a fleet,
  the same lock in the in-memory ledger), so a refusal at one level never leaves a reservation at the
  other. Settlement adjusts each level by the same amount.
- **The refusal** is the existing `429 attempt budget exhausted` with the existing
  `Retry-After` (the end of the window of the level that refused; the later of the two when both
  refuse). A new `level` (`"tenant"` or `"key"`) is added to the refusal's `metadata`, to the
  `llmshim_gateway_rejected_total` reason label (`budget_tenant`, `budget_key`) and to the access
  log's refusal line; the message text does not change and names no amount.
- **Windows** are independent: the tenant table has its own `budget_window_secs` and a key its own;
  the two may differ (a daily tenant cap over a monthly key cap is allowed and meaningful).
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
  limit; counters keep what they hold. Adding a table to a tenant that had key-level budgets
  **changes what the existing shared counter means** (it becomes the tenant counter, now limited by the
  table) and starts a fresh counter per capped key, so what the keys had already spent this window is
  counted against the tenant, not against any key. This is stated in the file's documentation and in
  the reload status; the migration is not silent.

## What it does not do

- No third level (project, team) in the file: `serve` has tenants and keys. The control plane has
  teams and projects (0024); mapping a team's budget onto the tenant level and a project's onto a
  future middle level is a later slice, and the design here is a **list of counters per request**
  (at most three) precisely so a middle level adds no new admission code.
- No budgets on tags (0029): a tag is a label. A cap per cost center would be a level, and it needs
  the tenant/team levels decided first.
- No alerts or notifications when a level nears its cap (a separate roadmap item; it needs a
  delivery channel and so its own note). `/v1/gateway/stats` gains each level's remaining headroom
  only if a later slice decides it is safe to expose.
- No borrowing between tenants, and no rollover of an unused window.

## Interaction with outage admission (0027)

A lease is a reservation inside one tenant counter. With a tenant table, **the lease is taken on the
tenant counter** as today, and a key that also has its own cap is **not admitted in outage state**
(its key counter cannot be reserved without Redis, and an in-memory stand-in could exceed the cap
after recovery). Such a key is refused with the existing outage reason. This is a stated limit, to be
revisited when a middle level exists. Keys with no cap of their own, under a tenant table, behave as
under 0027.

## Contract changes (each with its own test when built)

- **`[tenants.<name>]`** with `budget_usd`, `budget_window_secs`, `budget_allow_unpriced`, validated by
  path; the JSON Schema changes and `serve-config.schema.json` is regenerated.
- **Ledger keys**: the per-key counter (`...:key:{credential_prefix}:...`), its attempt records and
  index entries, in the same retention and tombstone accounting as today's (they count against
  `max_retained_attempts`; a request with two levels holds **one** attempt record that lists both
  counters, so the cap counts attempts, not counters).
- **The reservation and settlement scripts** take a list of counters (one to three) instead of one.
  The single-counter case is the same script path with a list of one, so existing behavior is the
  `n = 1` case, not a branch.
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
