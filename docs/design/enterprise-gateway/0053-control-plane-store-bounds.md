# 0053: Bounding the control plane's store

Status: built (first step). Follows the advisor's review of 0052.

## Problem

The control plane holds one database connection behind one mutex (`AppState.store`). Every API call, including the
gateways' usage ingest, queues for it. Two things could go wrong without any bound: a slow read (a 30-day summary, the
miss scan) holds the connection for as long as it likes, and every request behind it blocks a thread without limit.

## What is built

- **Bounded line.** `with_store` takes one of 64 permits before it waits for the lock, for at most 10 s. Past that the
  answer is 503 `store_busy` with `Retry-After: 5`. A slow query now delays at most the 64 requests in line; the rest
  are refused at once, and the gateways' exporter (which retries) is not left holding threads.
- **A statement budget on SQLite**, as Postgres already had (`statement_timeout` 30 s): a transaction running past 30 s
  is interrupted by a progress handler, which is cleared afterwards. `busy_timeout` is 5 s.
- The miss scan was already bounded (20,000 rows, `truncated` says so).

## Not built, and why

- **A separate read connection.** It would let reads proceed during a write. SQLite WAL and a second Postgres client
  allow it, but the in-memory test store cannot be shared between two connections, and every read method would have
  to move off the writer. It is the next step if the 503 shows up in `/metrics`-level use; the permits and the budget
  make that visible before it hurts.
- **A configurable queue length and wait.** Constants until a deployment needs otherwise.

## Tests

`a_full_queue_for_the_store_is_a_503_with_retry_after_not_a_hang` (api.rs),
`a_sqlite_transaction_past_its_budget_is_interrupted` (db.rs).
