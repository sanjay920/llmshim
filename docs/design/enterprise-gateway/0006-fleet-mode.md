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
`serve`, fleet mode reads nothing from the environment except the variables the
file names; a test starts it with the matching `LLMSHIM_*` variables set to
values that would break it.

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
- **All replicas must agree** on the provider list and limits; there is no
  version check between replicas. Deploy a configuration change as a rolling
  update and expect a window with mixed settings.
- **State survives restarts only in Redis.** Replay receipts are files on each
  replica (`state.receipts_dir`); a client that replays through a different
  replica needs that directory shared or sticky routing.
- **Upgrading from `gateway` with `LLMSHIM_REDIS_URL`** shares queues and buckets
  with `serve` replicas using the same Redis and provider names. Follow the
  transition rules in `docs/src/proxy/scaling.md` before mixing them.
