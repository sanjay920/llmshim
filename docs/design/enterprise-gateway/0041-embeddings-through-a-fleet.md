# 0041: Embeddings through a fleet

Status: built (embeddings only). Images, speech and transcription travel through the media object store instead (0050).

## Decision

With `[fleet]`, `POST /v1/embeddings` is run like a chat completion: the origin replica enqueues a job in the
shared Redis queue, **any replica's worker** takes it, sends the provider call under the fleet's shared rate,
concurrency and spend admission, and routes the answer back to the origin's HTTP connection. Before this note a
fleet answered `501` (0035), because a job's scope did not say what the job was and a worker would have run an
embeddings payload as chat.

The other three routes stay `501` on a fleet **on purpose, not for want of the same change**: their payloads or
answers are megabytes (a generated image's base64, synthesized audio, a 25 MB upload), and Redis is not where
that travels. A fleet that needs them can send those routes to a single-instance gateway; an object-store
hand-off is a separate design.

## How the operation travels

The job's `TrustedPolicyScope` gains `operation`, written **only when it is not chat**, and an embeddings scope is
**version 3**. A replica from before this change accepts scope versions 1 and 2 and does not recognise 3, so the
first such worker that takes the job **fails it with `503 coordinator unavailable`** (a terminal answer: the job is
not released to another replica, and nothing is retried). That is the safe failure: an embeddings call is never run
as chat and a chat answer is never returned to a caller who asked for vectors, and there is no loop. It also means
that during a rolling upgrade the share of embeddings requests that fail is roughly the share of workers still on
the old binary. A chat job's scope is byte-identical to before (no new field, version 1 or 2). Follow the
transition rule of 0030 and `docs/src/proxy/scaling.md`: **upgrade every replica before relying on embeddings
through the fleet**.

**Size.** The answer travels back through Redis, whose terminal limit is 64 MiB by default; a larger one would be
paid for upstream and then replaced by an error. A request on a fleet therefore holds **at most 512 texts** (a
`400` before anything is sent): 512 vectors of the widest model, 3072 dimensions, are about 20 MB of JSON. A single
instance has no such limit.

The caller's own refusals are unchanged and run on the origin before the job exists: authentication, the key's
allowlist (and residency), guardrails, the batch and model checks, and the spend refusals (a capped key needs a
trusted input price, 0035). The worker's dispatch reads the operation from the trusted context and calls the
embeddings dispatch, so the usage record, spend observation and breaker are the same code as on one instance, and
the record names the tenant whichever replica dispatched.

## Not built

- Images, speech, transcription and streaming failover through a fleet.
- A refusal at the origin when no replica of the fleet understands version 3 (the job fails with 503 at the first old
  worker that takes it), and releasing a refused job to another replica instead of failing it.

## Tests

`gateway::attempt::tests::an_embeddings_scope_is_version_3_with_its_operation_and_a_chat_scope_is_unchanged`
(unit): a chat scope has no `operation` field and keeps version 1, an embeddings scope is version 3 with
`operation` and round-trips, and a scope with no `operation` field reads as chat. The Redis-backed process tests
in `integration_serve_fleet` (two real replicas and a Redis): four embeddings requests, alternating replicas,
each answered with the provider's vectors and the provider hit exactly four times at `/embeddings` (not
`/chat/completions`), images, speech and transcription still answer `501`, and a batch of 513 texts is a `400` before the provider. The test shows the operation survives the trip through Redis, not which replica ran the call. Not serializing the operation fails the first.
The rolling-upgrade refusal by an *older* binary is the older code's own check and is not exercised.
