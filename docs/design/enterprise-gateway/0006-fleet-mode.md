# 0006: Fleet mode under `serve`

Status: implemented behind the `serve-redis` feature.

## What it is

```toml
[fleet]
redis_url = { env = "REDIS_URL" }   # redis:// or rediss://
```

With this table, a `serve` replica shares one Redis with its peers and behaves as
the existing `gateway` does under `gateway-redis`:

- one priority queue per provider, so any replica dispatches any job and returns
  the result to the replica that holds the caller's connection;
- provider rate-limit buckets, per-tenant limits and the spend ledger are shared,
  so a limit means the same thing on every replica;
- provider health (the circuit breaker) is shared;
- at-least-once delivery with leases, a reaper and a dead-letter queue.

It is built through the same `distributed_from_parts` constructor as the
environment path (`distributed_from_env` is that function applied to
environment-derived parts), so there is one implementation. Like single-instance
`serve`, fleet mode takes its settings from the file, not from `LLMSHIM_*`
variables: every serve test starts the server with the gateway, rate-limit and
fleet tuning variables set to values that would break it (a 1 ms Redis deadline,
a 1 ms job deadline and lease, one retained job), and the fleet tests pass. The
process-wide settings listed in 0002 (the shared HTTP client, the listener's
limits, the penalty after a 429) are the exceptions, as for single-instance
`serve`.

**Start order.** The queue workers lease jobs from peers, so they start only
after the configuration is valid, Redis is reachable and *both* listeners are
bound. A replica that fails to start (a port in use, say) therefore leaves no
lease on any peer's job; before this rule it could, and each lease spends one
of the job's delivery attempts and holds the job for the lease timeout. The
state is built by `distributed_unstarted` and the workers started by
`start_workers`; `distributed_from_parts`, which `llmshim gateway` uses, is the
two in sequence, so the gateway path is unchanged. Ordering is structural and
not covered by a test, because the window between a worker's first poll and the
process exiting cannot be observed deterministically.

## Building and running

The `[fleet]` table needs a binary built with `--features serve-redis`
(`serve` + `gateway-redis`). A binary without it reports the flag to rebuild
with, at `--check` and at startup, and starts nothing:

```
fleet: fleet mode needs a binary built with --features serve-redis
```

The Redis URL is a secret reference because it can carry a password. Errors never
contain it: the URL and any password inside it are removed from the message
before it is reported.

## What is proven

`tests/integration_serve_fleet.rs` starts two real server processes against one
Redis (a service container in CI):

- both serve traffic, and `/v1/gateway/stats` reports `distributed`;
- a tenant limit of three requests per minute lets exactly three of four
  alternating requests through, whichever replica takes them. A control test
  without Redis, with the same limit, lets all four through because each
  replica counts only its own;
- an unreachable Redis exits 1, announces nothing, and does not print the URL or
  its password.

Fleet tests give every test its own provider name and tenant: the queue and the
buckets are keyed by them, so two tests sharing a Redis would otherwise take
each other's jobs.

## Operating notes and limits

- **Redis outage.** The authenticated gateway's spend and rate coordination fails
  closed when Redis is unavailable, as documented for `gateway-redis`: requests
  are refused rather than admitted without accounting. A bounded local
  allowance for degraded operation is a roadmap item.
- **One Redis per environment.** Queues, rate buckets, health and the spend
  index are keyed by provider name and tenant only, with no fleet or environment
  prefix. Two fleets (staging and production, say) that share a Redis and a
  provider name would take each other's jobs, and a job runs under the upstream
  key of the replica that leases it. Give each environment its own Redis (or at
  least its own database number and provider names); a namespace setting is a
  roadmap item.
- **A stopped replica's leases.** On SIGTERM a replica drains its own requests
  but does not release leases it holds for peers' jobs; they return to the queue
  when the lease expires and the reaper requeues them, as under `gateway`.
- **All replicas must agree** on the provider list and limits; there is no
  version check between replicas. Deploy a configuration change as a rolling
  update and expect a window with mixed settings.
- **State survives restarts only in Redis.** Replay receipts are files on each
  replica (`state.receipts_dir`); a client that replays through a different
  replica needs that directory shared or sticky routing.
- **Upgrading from `gateway` with `LLMSHIM_REDIS_URL`** shares queues and buckets
  with `serve` replicas using the same Redis and provider names. Follow the
  transition rules in `docs/src/proxy/scaling.md` before mixing them.
