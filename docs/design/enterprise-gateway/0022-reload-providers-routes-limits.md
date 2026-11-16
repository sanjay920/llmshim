# 0022: Reloading providers, routes and rate limits

Status: design only. A contract note, written before any code (README working
rule 3). Nothing here is implemented; the slices are listed at the end. It extends
0012, which reloads `[[keys]]` and nothing else.

## What an operator needs

Three changes that today need a restart, and so a rolling deploy of every replica:

- **Rotating a provider's API key** (an upstream credential leaks, or its policy
  says ninety days).
- **Adding a provider or a route**, or repointing `route/compaction` at another
  model, without dropping the requests in flight.
- **Retuning a provider's requests or tokens per minute** after its quota changes.

## What reloads, and what does not

| Reloads (this note) | Stays fixed at start |
| --- | --- |
| `[providers.*]`: add, remove, change (keys, base URLs, cloud identities) | `[server]` (listener, TLS paths, trusted proxies, connection limits) |
| `[routes.*]` | `[admin]`, `[fleet]`, `[state]`, `[logging]`, `[telemetry]` |
| `[limits.rpm]`, `[limits.tpm]`, `[limits.providers.*]` | `[upstream]` (the client and its pool), `[auth.jwt]` |
| `[[keys]]` (already, 0012) | `limits.max_concurrency`, `limits.queue_timeout_ms`, `limits.penalty_secs`, `[reload]` itself |

The right-hand column is held by objects built once (a listener, a semaphore sized
to the concurrency, an exporter, the client's connection pool, the key store's
authentication mode) and stays refused **by path**, as 0012 refuses everything it
does not reload. The change in this note is to the left-hand column's objects.

## The contract

No new keys. A reload that changes anything in the left-hand column applies it;
one that also changes anything in the right-hand column is refused whole, by path,
as today.

## Rules

- **One snapshot per request.** The router becomes a value that is replaced as a
  whole, never edited in place. A request resolves its provider from the router in
  force when it **starts** and keeps that router through its retries, its fallback
  chain, its output-contract repair and its stream, so a reload never changes a
  request in flight (a stream that began on the old key finishes on it). Queued
  jobs are the one exception, stated plainly: a job waiting in a lane is dispatched
  on the router in force when it **leaves the queue**, so a provider that is removed
  while a job waits for it fails that job with a clear "unknown provider" error. It
  is never rerouted to another provider, and a wait is bounded by `max_wait`.
- **All or nothing, by building the whole thing.** The new router is fully built
  (every provider constructed from its table, every route resolved against it,
  every secret read) before anything is swapped. A provider that cannot be built is
  a rejection by path (`providers.azure.api_key`) and the previous router keeps
  serving. A route that names a provider the new file removes is refused (0001's
  rule: routes name a defined provider).
- **State that belongs to a provider's name survives.** The breaker (provider
  health), the rate buckets and the lanes are keyed by provider name, so a provider
  that is changed keeps them: a rotated key does not forget that the provider has
  been failing, and does not refill its bucket. The new router shares the existing
  breaker and the existing client (and with it the connection pool); only the
  provider objects and the routes are new. A **removed** provider's queue drains to
  the "unknown provider" error above; an **added** provider gets a lane when it is
  first used, as every lane does.
- **Rate limits are retuned in place.** The limiter's per-provider and global
  buckets take the new rate and capacity; the tokens in a bucket are kept, clamped
  to the new capacity, so a reload is not a limit reset (as for the tenants' limits
  in 0012). A provider that gains a limit gets a bucket; one that loses its limit
  loses its bucket. In a fleet the limiter is shared state in Redis; the rate and
  capacity are carried in each call, so each replica retunes with its own file (see
  "A fleet" below).
- **Secrets are read at reload and fingerprinted, never kept.** A provider's
  `api_key` reference is resolved during the reload, as caller keys are. The change
  fingerprint covers the **digest** of each resolved secret, so a rotated
  `{ file = ... }` key changes the result though the file is byte-identical, and the
  fingerprint, the status and every message carry no secret (0012's rule, extended
  to provider secrets). A provider secret that cannot be read **now** rejects a
  reload that changes that provider; an unreadable secret of a provider the reload
  does not touch is not a reason to refuse an unrelated change.
- **Cloud identities are replaced, not edited.** A changed Bedrock, Vertex or Azure
  table builds a new transport, so its token cache starts empty and the first
  request pays a token exchange. A changed `binding` is part of the replay boundary
  (0020): reasoning captured under the old binding is not replayed under the new
  one; that is the point of the field.
- **The same debounce as keys.** A poll applies a change only after two polls in a
  row have seen it; a signal never defers (0012). Provider and route files are read
  by the same watcher, and every file a provider table references is part of the
  fingerprint.
- **Status.** `/v1/serve/config` reports the generation, and for each reload what
  applied (providers added, removed or changed by name; routes; limits), never a
  value. A rejection names the path.

## A fleet

Each replica reloads from its own copy of the file, and the replicas do not
coordinate: for as long as a roll-out takes, replicas disagree. The rules that keep
that safe:

- **Add before you use; stop using before you remove.** Roll a new provider or
  route to every replica first, then send traffic that names it. A job leased by a
  replica that does not know its provider fails with the "unknown provider" error
  and is not retried onto a provider it did not name.
- **Limits converge, not jump.** Rates are applied per call from each replica's
  file, so during a roll-out the shared bucket is judged by whichever replica
  asks; it settles when the last replica has reloaded.
- A control plane that distributes configuration to every replica (0013, later)
  is the proper fix for skew; this note does not pretend to provide it.

## Failure modes (each gets a 0014 row and a test)

- A reload that adds a provider whose secret is unreadable: rejected by path; the
  old router keeps serving; nothing else in the same edit applies.
- A reload that removes a provider with queued jobs: they fail "unknown provider";
  requests in flight on it finish.
- A reload during a stream: the stream finishes on the key it started with.
- A reload that changes a provider's key and a rate limit in one edit: both apply
  together or neither does.
- Reloading faster than the debounce: the last complete file wins; a half-written
  file never applies (0012's two-poll rule).

## Not decided here

- Reloading the connection pool or any `[upstream]` setting (a new client replaces
  the pool; needs its own drain rule).
- Reloading `limits.max_concurrency` (a semaphore resized under load; needs the
  lanes' permits accounted for).
- Config-registered providers beyond the existing kinds (0013, later).

## Slices

1. This note.
2. **Routes.** A router's routes become replaceable without touching how providers
   are held (the routes are plain data resolved into a request at its start); the
   reload applies `[routes.*]`; tests: a route repointed between two requests, a
   request in flight unaffected, a route naming a removed provider refused.
3. **The router as a replaceable value and providers.** `GatewayState` and the proxy
   hold the router through a handle that yields a snapshot per request; the reload
   builds the new router (sharing the breaker and the client) and swaps it;
   queued-job dispatch uses the router at dequeue. Tests: a rotated key is used by
   the next request and not by one in flight, a stream survives a swap, an added
   and a removed provider, a provider that cannot be built rejects the whole
   reload, no secret in the fingerprint or the status, the fleet "unknown provider"
   failure.
4. **Rate limits.** The limiter retunes in place (in memory and in Redis);
   tests: a lowered rate applies to the next request and keeps the tokens, a
   removed limit removes the bucket, a reload is not a reset.
5. The 0014 rows, 0012's "keys only" text, the roadmap and CLAUDE.md.
