# 0022: Reloading providers, routes and rate limits

Status: all slices (2, 3, 3b, 4 and 5) implemented. Slice 2: the gateway holds its router behind a
replaceable handle and admission pins a snapshot that rides the request to its
dispatch. Slice 3: a reload rebuilds and swaps providers and routes in a single
instance. Slice 3b: on a fleet replica the same reload also starts workers for an added
provider and stops those of a removed one (below). Slice 4: a reload retunes the provider
rate limits in all four holders, on a fleet replica too. Slice 5 is the documentation
(the 0014 rows, 0012's status, the roadmap and CLAUDE.md). A contract note, written before
any code (README working rule 3). It extends 0012, which reloaded `[[keys]]` and
nothing else.

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
does not reload. A reload that changes anything in the left-hand column applies it;
one that also changes anything in the right-hand column is refused whole.

## What the code requires (found in review, and why the design is shaped this way)

- The router is an immutable value behind an `Arc` in `GatewayState` (and the
  proxy's `AppState`), and `Router::route_target` returns a borrow into its routes,
  so routes cannot be edited in place without breaking that public signature.
  **Routes and providers therefore reload by the same mechanism: the whole router
  is replaced.** There is no separate "routes first" step.
- The scheduler creates a lane for a provider lazily, but the **fleet** backend
  starts its workers and its reaper from the provider list taken **once at start**
  (`GatewayState::start_workers`). A provider added by a reload has no worker on a
  replica that started without it, so a reload must also start and stop workers.
- A provider's rate limits live in **four** places built from one `RateLimitConfig`:
  the scheduler's limiter, the attempt coordinator's local rates (which admit
  retries, repairs and fallbacks), and, in a fleet, the Redis limiter and the Redis
  attempt rates. Retuning one of them leaves the others on the old limit.
- A cloud provider (0020) reads **nothing** when it is built; its credentials load
  when a request is dispatched.

## Rules

- **One snapshot per admitted request.** The gateway holds the router through a
  handle that yields an immutable snapshot, replaced as a whole by a reload. A
  request resolves its provider, its route and its fallback chain against the
  snapshot in force when it is **admitted**, and **carries that snapshot with it**
  (the job holds it) through the queue, its retries, its fallback chain, its
  output-contract repair and its stream. So in a single instance a reload changes
  no request that was already admitted: a stream that began on the old key
  finishes on it, and a job queued for a provider that is then removed still runs on
  the provider it was admitted against. The removed provider's lane drains and is
  dropped when empty; its provider object lives as long as a job holds it.
- **In a fleet the guarantee is weaker, and says so.** A job crosses Redis and is
  run by whichever replica leases it, on **that replica's** snapshot. A replica
  that does not know the job's provider (a roll-out in progress, or a removal)
  answers `503 provider_unavailable` with `Retry-After`, **not** 502: it is a local
  condition, not an upstream failure, so it is not recorded by the breaker, not
  retried onto another provider, and no spend is reserved for it: the failure
  is at provider resolution, before any attempt exists, so nothing was sent and there
  is nothing to release. A fallback entry the worker's snapshot no
  longer resolves is skipped with the same classification, and the chain continues.
- **Built whole, then swapped.** The new router is fully built before anything is
  replaced: every provider constructed from its table, every route checked against
  the new provider set (a route may not name a provider the file removes, 0001),
  every secret read. A provider that cannot be built is a rejection by path
  (`providers.azure.api_key`) and the previous snapshot keeps serving.
- **What is checked at reload for a cloud provider.** The table is validated and
  built; for a Vertex provider whose table changed, the service-account file is
  read and parsed with the transport's own rules (the checks `--check` applies,
  0020), and a missing or malformed file rejects the reload. Bedrock's profile and
  Azure's key are not resolved until a request (the profile is the AWS SDK's to
  resolve): a reload can swap in an identity that then fails its requests, which is
  stated here rather than discovered.
- **Per-object atomicity, not global.** The router snapshot is swapped in one step;
  the limits are separate objects (below). A request admitted in the instant between
  the two swaps may see the new router with the previous limits, or the reverse.
  Neither a torn router nor a torn limiter is possible, and the reload applies the
  limits first, so a lowered limit takes hold no later than the provider that needs
  it. The status reports the generation of each.
- **State keyed by provider name survives.** The breaker (provider health), the
  rate buckets and the lanes are keyed by the entry's name (its admission identity,
  0032), which cannot change without removing the entry and adding another, so a changed
  provider keeps them (a renamed instance is a new one and starts fresh): a rotated key does not forget that the provider has been
  failing and does not refill its bucket. The new router shares the existing
  breaker and the existing client (and so the connection pool); only the provider
  objects and the routes are new. A renamed `openai-compatible` table is a removal
  plus an addition.
- **Workers follow the provider set.** In a fleet a reload starts the workers and
  the reaper for an added provider and, for a removed one, lets its workers finish
  what they hold and stop; queue-depth gauges and `/v1/gateway/stats` use the
  current provider set, with a removed provider's lane reported until it is empty.
- **Rate limits are retuned in all four places.** The limiter's rate and capacity
  become settable at each of the four holders; buckets keep their tokens, clamped to
  the new capacity, so a reload is not a limit reset (as for tenants in 0012). A
  provider that gains a limit gets a bucket; one that loses it loses its bucket. In
  Redis each call already carries the rate and capacity, so each replica's own
  config is what it presents. A test asserts that a retry, a repair and a fallback
  are admitted under the new limit as well as a first attempt.
- **Secrets: only `{ file }` can be rotated.** A provider's `api_key` reference is
  resolved during the reload and the change fingerprint covers the **digest** of the
  resolved secret, so a rotated file changes the result though the configuration is
  byte-identical, and neither the fingerprint, the status nor any message carries a
  secret (0012's rule, extended). An `{ env = ... }` reference is read from the
  process's environment, which cannot change while it runs, so it cannot be rotated
  by a reload; the status says which providers' secrets are of that form so an
  operator is not left believing a rotation applied. A provider secret that cannot
  be read now rejects a reload that changes that provider; an unreadable secret of a
  provider the reload does not touch is not a reason to refuse an unrelated change.
- **Cloud identities are replaced, not edited.** A changed Bedrock, Vertex or Azure
  table builds a new transport, so its token cache starts empty and the first
  request pays an exchange. A changed `binding` is part of the replay boundary
  (0020): reasoning captured under the old binding is not replayed under the new.
- **The same debounce as keys.** A poll applies a change only after two polls in a
  row have seen it; a signal never defers (0012). Every file a provider table
  references is part of the fingerprint.
- **Status.** `/v1/serve/config` reports the generation and, for each reload, what
  applied by name (providers added, removed, changed; routes; limits) and which
  provider secrets are env-form, never a value. A rejection names the path.

## A fleet

Each replica reloads from its own copy of the file, and replicas do not coordinate:
for as long as a roll-out takes they disagree. What keeps that safe: **add before you
use, stop using before you remove**; limits converge when the last replica has
reloaded (each call presents its replica's rate, so the shared bucket is judged by
whichever replica asks meanwhile). A control plane that distributes configuration
(0013, later) is the proper fix for skew; this note does not pretend to provide it.

## Failure modes (each gets a 0014 row and a test)

- A reload that adds a provider whose secret is unreadable: rejected by path; the old
  snapshot keeps serving; nothing else in the same edit applies.
- A reload that removes a provider with queued jobs: in a single instance they finish
  on the provider they were admitted against; in a fleet a replica without it answers
  503 `provider_unavailable`.
- A reload during a stream: the stream finishes on the key it started with.
- A reload that changes a provider's key and a rate limit in one edit: both apply
  (limits first); a request in the instant between sees one new and one old, never a
  torn object.
- A reload that adds a provider in a fleet: its workers start on this replica.
- Reloading faster than the debounce: the last complete file wins; a half-written file
  never applies (0012's two-poll rule).

## Not decided here

- Reloading the connection pool or any `[upstream]` setting (a new client replaces the
  pool; needs its own drain rule).
- Reloading `limits.max_concurrency` (a semaphore resized under load; needs the lanes'
  permits accounted for).
- Config-registered providers beyond the existing kinds: **decided against** (2026-11-17). A provider is a kind plus a `base_url`: any endpoint that speaks a wire llmshim has (OpenAI Chat Completions, the Responses API, Anthropic Messages, Gemini, and the cloud transports) is registered with the kind of that wire and its address (`base_url`; `region` or `endpoint` for the cloud kinds, which refuse `base_url`), and a wire it does not have is a library adapter with its own translation and tests, which a configuration file cannot supply.
- The proxy (`llmshim proxy`) keeps its own router and does not reload: its
  `AppState.router` is a public field, and nothing in this note changes it.

## Decided in slice 3

- **An unreadable secret of an unchanged provider does not block an unrelated edit.**
  Its secret digest is carried over while its table is unchanged. If the same edit
  also changes another provider, the router is rebuilt whole and the unreadable
  secret is reported by path, rejecting the reload: a router cannot be built without
  every provider, and the running provider objects are not reusable.
- **Known limits, stated rather than discovered.** A secret file deleted while its
  provider's table is unchanged keeps the running key and shows no change in the
  status. Every provider is rebuilt on any router change, so an unchanged cloud
  provider's token cache is also dropped (one extra token exchange). A changed or added
  Vertex provider whose service-account file is missing, empty or malformed rejects
  the reload.
- **Slice 4 as built.** `RateLimiter::retune` and `AttemptRates::retune` are defaulted
  methods (so no implementor breaks); each of the four holders keeps its limits behind
  a small read-mostly lock (`LiveLimits`) and reads them on every call. A bucket follows
  a changed limit at its next use: time so far is credited at the old rate and the
  balance is clamped to the new capacity (a lowered limit bites at once, a raised one
  refills from what is there); a limit that is removed drops its bucket and one that
  returns starts fresh; an unchanged limit is not touched, so a reload that does not
  change the limits (or does not change a provider's) is not a reset. The Redis holders
  present this replica's rate and capacity on every call, and their script clamps a
  lowered capacity and keeps the tokens. A bucket dropped
  with its limit takes an active penalty with it. In Redis a removed limit leaves the
  stored bucket to expire (an hour of inactivity), so a limit that returns within that
  time resumes the balance it had, clamped to the new capacity, rather than starting
  full. In memory, buckets of providers without a limit are swept at the reload (or at
  the next call when the map is busy), once, so an unlimited provider's calls never
  pay for it. Limits are applied before the router. They are
  in the fingerprint and debounce with everything else, apply on a fleet replica (each
  replica presents its own file's rate), and have their own `limits_generation` beside
  `router_generation`. `[limits]` keeps `max_concurrency`, `queue_timeout_ms` and
  `penalty_secs` fixed at start, refused by path as before. In `serve`, the coordinator's
  limiter (not the scheduler's) is the one that admits every attempt, the first
  included (a policy-gated job skips the scheduler's), so the process test covers a
  first attempt and the unit tests the other holders.
- **The status** gains `router_generation`, `env_secret_providers` and, on a
  reload, `changes` (`providers.added: x`, `providers.removed: x`,
  `providers.changed: x`, `routes`).
- **Files in the fingerprint**: a provider's `api_key` file, `service_account_file`,
  and the Bedrock `config_file`/`credentials_file`, by digest.

## Slice 3b as built (fleet workers)

- `DistributedGateway` keeps a registry of the workers it started (`ManagedWorkers`):
  one preparation semaphore per provider, shared by its two protocol workers. The start
  (`start_managed_workers`, once, after every listener is bound) reads the provider set
  at that moment; `sync_workers` follows it afterwards, and a sync that arrives before
  the start is a no-op (so a reload cannot start workers early).
- An added provider gets workers at once. A removed provider's semaphore is **closed**:
  its workers end at the next turn of their loop, and a leased job already running holds
  its own permit in its own task, so it finishes. Jobs left in the queue for a removed
  provider are served by whichever replica still has it. The reaper reads a provider
  list that only grows, so expired leases of a removed provider are still redelivered.
- The reload calls the sync after the router is replaced, so a job leased for an added
  provider finds it in the router.
- **503 `provider_unavailable`.** A job whose provider this replica's router does not know
  (a roll-out in progress, or a removal) is answered 503 with `Retry-After` (the
  configured penalty) and the code `provider_unavailable`, never 502. It fails at
  provider resolution, before any attempt exists, so nothing was sent, no spend was
  reserved (nothing to release) and the breaker, which observes the client's result,
  never sees it. A fallback entry the router does not resolve was already skipped by the
  chain (`fallback.rs` records it and continues). The new `ApiError::ProviderUnavailable`
  variant and the dispatch mapping exist only in a `serve` build: `llmshim gateway` keeps
  its 502.
- The start reads the provider list under the registry's lock, so a reload that lands
  during startup syncs after it. The 503 covers a provider the router does not resolve;
  a route the replica does not yet have fails route expansion and an all-unknown
  fallback chain ends as `all_failed`, neither a 503 (a roll-out gap, stated here).
- Process test (`integration_serve_fleet`): a provider added by a reload is served by
  the replica from then on; unit test against Redis: workers start for an added provider
  and stop leasing for a removed one.

## Slices

1. This note.
2. **Done: the router handle.** `GatewayState` holds the router through a handle yielding
   snapshots; admission takes a snapshot and the job carries it; dispatch, fallback,
   repair and streaming use the job's snapshot, never the handle. Nothing swaps yet,
   so every existing test must pass unchanged (the gateway and `llmshim gateway`
   behave as before). New tests: a swap between admission and dispatch does not change
   an admitted request (provider, route, fallback chain), a queued job for a removed
   provider finishes, a stream survives a swap.
3. **Done (the fleet part is 3b, also done): reload of providers and routes.** The reload builds the new router (sharing the
   breaker and the client), checks the credential files of changed Vertex providers,
   fingerprints resolved provider secrets by digest, swaps the handle, and starts and
   stops fleet workers; the status reports what applied and which secrets are
   env-form. Tests: a rotated `{ file }` key is used by the next request and not by
   one in flight; an added and a removed provider; a provider that cannot be built
   rejects the whole reload with no secret in the fingerprint or the status; an
   env-form secret is flagged; the fleet 503 `provider_unavailable` and its
   classification (503, `Retry-After`; no attempt exists to observe or reserve); an added provider's
   worker starts.
4. **Done: rate limits.** The four holders become retunable; the reload applies the limits
   before the router. Tests: a lowered rate applies to the next request, including
   retries, repairs and fallbacks, and keeps the tokens; a removed limit removes the
   bucket; a reload is not a reset; Redis presents the new rate.
5. **Done:** the 0014 rows, 0012's "keys only" text, the roadmap and CLAUDE.md.
