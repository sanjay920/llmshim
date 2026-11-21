# 0053: Bounding the control plane's store

Status: built (first step). Follows the advisor's review of 0052.

## Problem

The control plane holds one database connection behind one mutex (`AppState.store`). Every API call, including the
gateways' usage ingest, queues for it. Two things could go wrong without any bound: a slow read (a 30-day summary, the
miss scan) holds the connection for as long as it likes, and every request behind it blocks a thread without limit.

## What is built

- **Bounded line, bounded time.** `with_store` takes one of 16 permits, and the usage batches one of 64 on a line of
  their own, so a burst of dashboard reads cannot take a gateway's turn (a batch waits behind at most 16 reads). One
  10 s deadline covers the wait for a permit and then for the connection; past it the answer is 503 `store_busy` with
  `Retry-After: 5`, and the work never runs, so a caller already told to retry does not also have its write happen.
  The lock is polled, not awaited. Still not a latency cap on work already running: a request holds the connection for
  at most the statement budget.
- **A statement budget on SQLite**, as Postgres already had (`statement_timeout` 30 s): a transaction running past 30 s
  is interrupted by a progress handler, cleared after the transaction even if the work panics. It surfaces as
  503 `store_timeout` (`Error::Timeout`, nothing written), not a 500. `busy_timeout` is 5 s, set first.
- The miss scan was already bounded (20,000 rows, `truncated` says so).

## Not built, and why

- **A separate read connection.** It would let reads proceed during a write. SQLite WAL and a second Postgres client
  allow it, but the in-memory test store cannot be shared between two connections, and every read method would have
  to move off the writer. It is the next step if the 503 shows up in `/metrics`-level use; the permits and the budget
  make that visible before it hurts.
- **A configurable queue length and wait.** Constants until a deployment needs otherwise.

## Tests

api.rs: a full queue is a 503 with Retry-After, usage batches have their own line, work is not run past the deadline,
the budget is a 503. db.rs: a transaction past its budget is `Timeout` and the next one is not cut, the handler is
cleared when the work panics (a thread-local test budget drives the production path).
