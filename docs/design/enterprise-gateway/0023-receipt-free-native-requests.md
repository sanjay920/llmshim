# 0023: Native requests and answers that need no replay receipt

Status: design. A contract note, written before any code (README working rule 3).
The change is behavioural, not a new setting: nothing in the configuration file,
the HTTP API or the receipt store's on-disk format changes.

## The problem

The native inbound wires (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`,
the Gemini paths) translate a request to the canonical chat shape and an answer
back, and persist or restore *replay receipts* (a reasoning block's signature, a
tool call's wire ids) so a later turn can be replayed faithfully (0013's
"Multi-model conversations", `CLAUDE.md`). Receipt work is blocking file I/O, so
it runs on a small executor (`src/proxy/wire/mod.rs`): **two** workers, **one**
at a time for ingress, **8** ingress and **4** egress slots. A slot is taken
without waiting; the worker permits are then awaited for up to five seconds. A
request that cannot get them is answered `503 native replay metadata is busy;
retry the request` with `Retry-After: 1`, and a store lock that times out maps
to the same 503.

The benchmark (0018) measured the consequence: above a dozen requests in flight
the native `/v1/chat/completions` wire answered 503 to 15 % of requests at 32
concurrent and 27 % at 64, each in about half a millisecond, and this is now the
dominant limit on that wire. 0018 supposed that "the native wire stores replay
metadata for every answer". Reading the code shows the cause is narrower:

- **Egress** (`response_from_chat`): no wire calls `Receipts::put` for an
  ordinary text answer. A tool call is stored for every wire
  (`response.rs`, before the wire dispatch), and a reasoning block on the Chat,
  Messages, Gemini (text thoughts only) and Responses (`responses::retain`) wires.
  A text answer takes an egress slot and a blocking-thread hop anyway, because the
  whole conversion is handed to the executor.
- **Ingress** (`request_to_chat_with_limits`): the reads, per wire, are
  - Chat: `get_bounded` for **every assistant message** (a reasoning lookup, whether
    or not a handle is present) and for every call in its `tool_calls`
    (`import_call` looks the id up before it checks the `call_ls_` prefix, so a
    legacy client id reads the store too);
  - Messages: every `tool_use`, and `thinking`/`redacted_thinking` blocks;
  - Gemini: only `thought` parts (`functionCall` ids are derived, with no read);
  - Responses: the Chat path, plus `x-responses-reasoning` items.
  So a first turn, or a history with no assistant turn, never reads the store; **any
  multi-turn Chat or Responses conversation does**. The benchmark sends one user
  message (`serve_bench.rs`), which is why it is the free case, and why this
  change cannot be assumed to help a deployment whose callers replay history: for
  those, ingress is unchanged and only egress (text answers) is freed.
- **The Responses wire's `replay_metadata` step** takes a second ingress slot (and
  the single ingress worker) but does **no receipt I/O**: it transforms the request
  for the target, filters reasoning and merges messages. It is pure.

So the executor is occupied by work that does no receipt I/O at all, in the
benchmark's case entirely and in a replaying caller's case for every text answer
and for the Responses metadata step.

## The rule

**Run the conversion first without any store; fall back to the executor only if
the conversion actually asks for the store.**

- `Receipts` gains a private *tripwire* mode (`Receipts::tripwire()`): a value with
  no root whose `get`, `get_bounded` and `put` return an error **before touching
  anything** (`get_inner` and `put` check the mode first). It is a `Receipts`, not a
  new trait, because the conversions take `&Receipts`. It records that it was hit
  in an `AtomicBool` that `run_free_first` reads: the decision never parses an
  error message, so a step that wraps or reformats errors cannot turn a tripwire
  hit into a user-visible 400. The tripwire **never returns `Ok(None)`**: the
  Responses restore reads `None` as an expired receipt and drops the item, and the
  Chat import reads it as a legacy id and passes the call through, each a silent
  wrong success.
- `ReceiptExecutor::run_free_first(kind, operation)` calls
  `operation(&Receipts::tripwire())` **inline**, on the request's own task, inside
  `catch_unwind` (the executor's `spawn_blocking` turns a panic into a fixed
  `Failed` answer today; inline must not unwind the request task). If the tripwire
  was not hit, that result is the result: success, or an ordinary error returned as
  the executor's `Failed` exactly as today. If it was hit, the same operation runs
  again through the executor against the request's real store (an explicit
  `Arc<Receipts>` extension still wins), with the same slots and the same refusal
  when busy.
- Ingress, unary egress and the streaming egress of the Chat, Messages and Gemini
  wires use it (the Gemini stream shares the unary path's `response_from_chat` call).
  The Responses wire's streaming egress and `retain` use it too. The Responses
  `replay_metadata` step, being pure, runs **inline with no executor** at all.

The decision is made by the code that would do the I/O, not by a predicate written
beside it, so it cannot disagree with it: there is no list of "receipt-bearing"
keys to keep in step with the conversion.

## What it must not change

- **The bytes.** For a request that needs the store, the result is the one the
  executor produced before; the inline attempt's partial work is discarded. A
  test replays the same corpus both ways and compares.
- **Failure and refusal behaviour** for any request that needs the store:
  Busy, the lock timeout, the request-size limit and every error keep their
  statuses. Only requests that never needed the store stop being refused for want
  of a slot.
- **Fail-closed identity rules.** A tool result whose owned id has no receipt
  still errors (that is a `get` that returns nothing, and `get` is the tripwire).
- **Default, proxy, gateway and gateway-redis builds** change only in that the
  native wires stop taking executor slots for receipt-free work, which is the point;
  nothing about receipts that exist or are required changes.

## Cost, and what the executor provided besides I/O

- **Doubled CPU** only for a request that needs the store (the inline attempt is
  discarded), which also does blocking I/O; small next to it.
- **Concurrency.** The single ingress worker serialised all ingress conversion;
  inline, conversion runs concurrently, bounded by the body limit, the JSON
  complexity limit and `enforce_canonical_bounds`. A conversion can inflate (the
  Messages path clones, the Responses restore expands), so peak memory per request
  exceeds the body: the benchmark's memory column is re-read after the change.
- **Cancellation.** The executor stops queued work after a client disconnect; inline
  there is no queue to wait in.
- **The lock timeout** (`with_http_lock_timeout`) is thread-local and set only inside
  the executor. The tripwire never locks, so it is not needed inline. **Invariant:**
  a real store is never used inline, because its lock acquisition would fall back to
  a blocking `lock_exclusive` on an async worker.
- **A receipt-free request no longer creates the store directory or its lock file**
  (`get` created them); a test asserting that the root exists after any request
  changes.

## Tests

- A tripwire `Receipts` refuses `get`, `get_bounded` and `put` without touching the
  filesystem (the root need not exist) and never returns `Ok(None)`.
- A text-only exchange (first turn; text answer) completes with every executor slot
  held on the Chat, Messages and Gemini wires; the Responses `replay_metadata` step
  takes no slot.
- A multi-turn Chat request (an assistant message), a request with a tool result or
  reasoning handle, and an answer with a tool call or reasoning block still go
  through the executor, are refused when it is saturated, and produce the same
  bytes as before (a corpus replayed both ways).
- Mutation checks: a tripwire that returns `Ok(None)` must fail **both** the "tool
  result with an owned id still errors" test and the "reasoning handle is not
  silently dropped" test.
- A conversion that panics returns the executor's fixed failure, inline too.
- The benchmark is re-run (0018's method), recording the first-turn case it sends
  and, new, a multi-turn case so the gain is not overstated.

## Not decided here

- Whether the executor's slot counts should be configurable (`[limits]`), or its
  refusal replaced by a short wait. With this change they matter only to requests
  that carry receipts.
- Receipt storage that is not local files (a fleet shares none today: 0006).

## Slices

1. This note.
2. The tripwire, `run_free_first`, its use on the chat, Messages and Gemini
   paths, and the tests above.
3. The benchmark re-run recorded in 0018, and the roadmap line.
