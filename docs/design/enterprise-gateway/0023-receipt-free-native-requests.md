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
at a time for ingress, **8** ingress and **4** egress slots, each class taken
**without waiting**; a request beyond them is answered `503 native replay
metadata is busy`.

The benchmark (0018) measured the consequence: above a dozen requests in flight
the native `/v1/chat/completions` wire answered 503 to 15 % of requests at 32
concurrent and 27 % at 64, each in about half a millisecond, and this is now the
dominant limit on that wire. 0018 supposed that "the native wire stores replay
metadata for every answer". Reading the code shows the cause is narrower and the
fix therefore larger in effect:

- **Egress** (`response_from_chat`): the chat and Messages wires call
  `Receipts::put` only for a tool call or a reasoning block. An ordinary text
  answer never touches the store, yet it still takes an egress slot and a
  blocking-thread hop, because the whole conversion is handed to the executor.
- **Ingress** (`request_to_chat_with_limits`): `Receipts::get` is called only for
  a request that carries an owned tool id or a reasoning handle to restore. A first
  turn, or any turn whose history is plain text, never reads the store, yet it
  takes an ingress slot (and the single ingress worker, so ingress is serial).

So the executor is occupied by work that does no receipt I/O at all. Both
conversions are otherwise pure and cheap.

## The rule

**Run the conversion first without any store; fall back to the executor only if
the conversion actually asks for the store.**

- `Receipts` gains a private *tripwire* mode (`Receipts::tripwire()`): a value
  with no root whose `get`, `get_bounded` and `put` return a fixed
  `needs-a-receipt` error before touching anything.
- The executor gains `run_free_first(kind, operation)`: it calls
  `operation(&Receipts::tripwire())` **inline**, on the request's own task. If that
  succeeds, or fails for any reason other than the tripwire, that result is the
  result (an ordinary error is returned as the executor's `Failed`, exactly as
  today). If it fails *because* the tripwire was hit, the same operation runs
  again through the executor against the real store, exactly as it does today
  (same slots, same refusal when busy).
- Ingress, unary egress and the streaming chat/Messages egress use it. The
  Responses wire's replay-metadata step and its streaming egress keep the
  executor: they write item snapshots for every answer by design.

The decision is made by the code that would do the I/O, not by a predicate
written beside it, so it cannot disagree with it: a conversion that needs a
receipt reaches `get`/`put` and is sent to the executor, one that does not never
does. There is no list of "receipt-bearing" keys to keep in step with the
conversion.

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

## Cost, and why inline is acceptable

The inline attempt runs conversion on the async task. It is bounded by the same
request-size and JSON-complexity limits that already apply to parsing, which is also
inline; for a request that then needs the store the conversion is done twice (the
second time on the blocking pool). That doubled CPU applies to requests that
carry tool calls or reasoning, which also do blocking I/O, so it is small next to
them. It is measured, not assumed (below).

## Tests

- A tripwire `Receipts` refuses `get` and `put` without touching the filesystem
  (the root need not exist).
- Ingress and egress of a text-only exchange complete with the executor
  saturated (every slot held): they were refused before.
- A request with a tool result or reasoning handle, and an answer with a tool call
  or reasoning block, still go through the executor and are refused when it is
  saturated, and produce the same bytes as before.
- A mutation check: making the tripwire permissive (it returns `Ok(None)`)
  must fail the "tool result with an owned id still errors" test.
- The benchmark is re-run (0018's method) and the row for the native wire under
  concurrency is recorded beside the first two runs.

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
