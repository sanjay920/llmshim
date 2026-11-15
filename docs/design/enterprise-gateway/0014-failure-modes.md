# 0014: What happens when a dependency fails

Status: a standing record, extended with every step that adds or changes a
dependency (0013, bar item 1). The first rows are tested against real processes.

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

| Dependency and event | What the replica does | Test |
| --- | --- | --- |
| Redis unreachable at start (fleet) | exits 1 before serving; the message names `fleet.redis_url`, never the URL or its password | `integration_serve_fleet::an_unreachable_redis_fails_the_start_without_leaking_the_url` |
| Redis lost while serving | a new request is refused with a 5xx at once (under a millisecond in the recorded run), **never admitted**: the provider is not called, so no spend goes unaccounted; `/ready` answers 503; the replica stays up | `integration_serve_outage::redis_outage::losing_redis_refuses_work_instead_of_admitting_it_and_recovery_needs_no_restart` |
| Redis lost while a request is with the provider | the caller is answered with a 503 and not left waiting for a result that can no longer be delivered (5.3 s in the recorded run; the test allows 30 s); the replica stays up | `…::losing_redis_mid_request_ends_the_call_promptly_and_the_replica_recovers` |
| Redis returns | the replica serves again with no restart (0.6 s after the connection returned in the recorded run; the tests allow 60 s), and `/ready` answers 200 | both tests above |
| `SIGTERM` | a request already in flight finishes with its answer, the listener stops accepting at once, the process exits 0 | `integration_serve_outage::sigterm_lets_an_in_flight_request_finish_then_exits_zero` |
| A provider answers with errors or none | the client retries, each attempt is its own usage record (`http_failure`, unknown cost, never free), and the caller gets the provider's error | `integration_serve::every_failed_attempt_is_recorded_with_an_unknown_cost` |
| A secret is unset, blank or unreadable at start | exits 3, naming the setting; a secret whose value breaks a rule exits 1 and outranks it | `unit_serve_cli` |
| A keys reload that does not load, or changes more than keys | refused by path, nothing changes, the previous keys keep serving; `/v1/serve/config` says `in_sync: false` | `integration_serve_reload` |
| A TLS replacement that does not load | the previous certificate stays in service | `integration_serve_tls::a_replacement_that_does_not_load_keeps_the_previous_certificate` |
| A log sink that stalls or fails while serving | the writer queue absorbs it; overflow is dropped and counted, a failed write is counted, neither reaches the request | `serve::jsonl` unit tests (`a_stalled_destination_never_blocks_the_caller_and_overflow_is_counted`, `a_failing_destination_is_counted_and_never_reaches_the_caller`) |
| A log path that cannot be written at start | exits 1 naming `logging.access_log` or `logging.usage_log`; nothing is created | `integration_serve::an_unwritable_access_log_fails_the_start_with_the_setting_named` |

The numbers are one run on a developer machine, to show the order of magnitude;
the tests assert the bounds, not the figures.

## Not yet tested

Each of these has behavior (often documented elsewhere) but no test that breaks
the dependency, so none of it is a promise yet.

- **The provider circuit breaker** at the server level. The library tests pin its
  states; the cooldown (30 s by default) makes a process-level test slow, and the
  breaker's settings are not in the configuration file yet.
- **Redis slow rather than gone**, and a partition between replicas rather than
  between a replica and Redis. The outage tests cut every connection; a stalled
  connection exercises the operation deadlines instead.
- **A replica killed with `SIGKILL`** while it holds leases for other replicas'
  jobs. Library tests cover origin crash and lease expiry against a real Redis,
  and 0006 says the jobs wait out the lease; there is no two-process test.
- **The receipts directory becoming unwritable** after the start.
- **A full disk under a log file**, which the writer counts as a failed write;
  not simulated at process level.
- **Rotation of a provider, admin or Redis secret**: it needs a restart (0012),
  and nothing tests that a running server does not notice the change.

0013's fifth stability criterion needs this section empty (or each item here
turned into a row with a test).

## How the Redis tests cut Redis

`RedisProxy`, in `tests/integration_serve_outage.rs`, forwards TCP to the real
Redis and can cut every open connection and refuse new ones, then restore. A
replica sees what it would see if Redis, or the network to it, went away. The
replica is pointed at the proxy, so the real Redis is untouched and other tests
sharing it are not disturbed. The tests run in the `serve-redis-check` job
against a Redis service container.
