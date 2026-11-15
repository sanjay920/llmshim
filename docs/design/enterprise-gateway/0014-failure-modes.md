# 0014: What happens when a dependency fails

Status: a standing record, extended with every step that adds or changes a
dependency (0013, bar item 1).

## Why a table

An unattended caller cannot ask what a refusal meant. For each thing `llmshim
serve` depends on, this says what a replica does when it is down, slow or wrong,
and which test fails if that stops being true. A row with no test says so: the
rows in "Not yet tested" are not claims.

The rule behind the rows (0013): on the inference path, identity and money fail
closed (a request whose caller cannot be identified, or whose spend cannot be
reserved, is refused); telemetry fails open (a log sink or exporter never blocks
or fails a request). At startup the opposite holds: a dependency that is needed
and absent stops the start, with a status that says which kind of absence it is.

## Tested

Each row names its test. "Process" means a real `llmshim serve` process was
started and the dependency broken under it; "unit" means the component alone.

| Dependency and event | What the replica does | Test |
| --- | --- | --- |
| Redis unreachable at start (fleet) | exits 1 before serving; the message names `fleet.redis_url`, never the URL or its password | process: `integration_serve_fleet::an_unreachable_redis_fails_the_start_without_leaking_the_url` |
| Redis lost while serving | a new request is refused with a 5xx at once (under a millisecond in the recorded run; the test allows 3 s, well inside the 5 s Redis deadline). Every fleet request goes through the Redis queue, so it cannot be queued: it is **never admitted** and the provider is not called (the mock must have been hit exactly once, before the outage). `/ready` answers 503, `/health` stays 200, the replica stays up | process: `integration_serve_outage::redis_outage::losing_redis_refuses_new_work_and_recovery_needs_no_restart` |
| Redis lost while a request is with the provider | the caller is told the request failed (a 5xx; a 503 after 5.3 s in the recorded run; the test allows 30 s) and is not left waiting for an answer that cannot be delivered. The replica stays up. *The 503's body says `gateway shutting down`, which is the wrong words for a lost Redis; the status is the right one to retry on.* | process: `…::losing_redis_mid_request_ends_the_call_promptly_and_the_replica_recovers` |
| Redis returns | the replica serves again with no restart, and `/ready` answers 200 (0.6 s after the connection returned in the recorded run; the tests allow 60 s) | both Redis tests above |
| `SIGTERM`, one replica | with a request in flight (held by the provider until the test releases it) both listeners, main and admin, stop accepting at once while the request is still pending; the request then finishes with its answer; the process exits 0 and says it received the signal | process: `integration_serve_outage::sigterm_lets_an_in_flight_request_finish_then_exits_zero` |
| A provider answers 500 | the client retries; each attempt is its own usage record (`http_failure`, unknown cost, never free); the caller gets a 5xx | process: `integration_serve::every_failed_attempt_is_recorded_with_an_unknown_cost` |
| A secret is unset, blank or unreadable at start | exits 3, naming the setting; a secret whose value breaks a rule exits 1 and outranks it | process: `unit_serve_cli` |
| A keys reload that does not load, or changes more than keys | refused by path, nothing changes, the previous keys keep serving | process: `integration_serve_reload`. That `/v1/serve/config` then says `in_sync: false` is pinned by the `serve::reload` unit tests |
| A TLS replacement that does not load | the previous certificate stays in service | process: `integration_serve_tls::a_replacement_that_does_not_load_keeps_the_previous_certificate` |
| The admin port is taken at start | exits 1 before serving, naming the listener | process: `integration_serve::a_taken_admin_port_fails_the_start_cleanly` |
| A log path that cannot be written at start | exits 1 naming `logging.access_log` or `logging.usage_log`; nothing is created | process: `integration_serve::an_unwritable_access_log_fails_the_start_with_the_setting_named` |
| A log sink that stalls or fails while serving | the writer queue absorbs it; overflow is dropped and counted, a failed write is counted, neither reaches the request | unit: `serve::jsonl` (`a_stalled_destination_never_blocks_the_caller_and_overflow_is_counted`, `a_failing_destination_is_counted_and_never_reaches_the_caller`) |
| A settlement that cannot be written to the spend ledger | the liability the attempt reserved is kept, not released | unit (against a real Redis): `gateway::attempt::tests::redis_failed_settlement_keeps_the_acquired_liability` |

The numbers are one run on a developer machine, to show the order of magnitude;
the tests assert the bounds, not the figures.

## Not yet tested

Each of these has behavior (often documented elsewhere) but no test that breaks
the dependency, so none of it is a promise yet.

- **The money path in isolation under Redis loss.** The Redis-loss tests refuse
  at the queue, which every fleet request needs, so they do not reach the spend
  ledger or the rate limiter. Two components are documented as failing **open**
  when Redis is unreachable: the shared rate limiter used by the proxy
  (`RedisRateLimiter`: "admits the request and logs") and the older per-tenant
  `SpendStore` ("a Redis failure loses the charge rather than the request").
  0006 says spend and rate coordination fail closed. Which of these `serve` can
  reach has to be established and the answer tested, or 0006 corrected; until
  then this note claims only what the rows above show.
- **`SIGTERM` in a fleet.** A replica drains its own requests but does not
  release the leases it holds for peers' jobs, which wait out the lease timeout
  (0006); no two-process test. Also untested: `SIGTERM` while a provider hangs
  (the drain has no deadline of its own), and a signal that arrives between the
  banner and the handler being installed.
- **The provider circuit breaker** at the server level. The library tests pin
  its states; the cooldown (30 s by default) makes a process-level test slow, and
  the breaker's settings are not in the configuration file yet. Likewise a
  provider that refuses connections, hangs, or answers 429.
- **Redis slow rather than gone**, a partition between replicas rather than
  between a replica and Redis, a Redis that restarts with its data lost, one that
  rejects authentication or answers `READONLY`, and a request streaming when
  Redis is lost. The outage tests cut every connection; a stalled one exercises
  the operation deadlines instead.
- **A replica killed with `SIGKILL`** while it holds leases. Library tests cover
  origin crash and lease expiry against a real Redis; there is no two-process
  test.
- **The main listener's port being taken at start** (the admin listener's is
  tested).
- **The receipts directory becoming unwritable**, the **configuration or a
  secret file being removed or rotated** while the server runs, **log rotation**,
  **a full disk under a log**, **wall-clock changes under spend windows and
  certificate validity**, and **DNS**.

0013's fifth stability criterion needs this section empty (or each item here
turned into a row with a test).

## How the Redis tests cut Redis

`RedisProxy`, in `tests/integration_serve_outage.rs`, forwards TCP to the real
Redis and can cut every open connection and refuse new ones, then restore. A
replica sees what it would see if Redis, or the network to it, went away. The
replica is pointed at the proxy, so the real Redis is untouched; but a replica
abandoned in the middle of a request leaves a lease that other tests' replicas
would sweep (the origin-lease index is Redis-wide), so these tests run one at a
time (`--test-threads=1`) in the `serve-redis-check` job, after the fleet tests.
