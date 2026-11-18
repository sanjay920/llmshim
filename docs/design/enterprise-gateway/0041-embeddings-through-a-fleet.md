# 0041: Embeddings through a fleet

Status: built (embeddings only). Images, speech and transcription still answer `501` on a fleet.

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
**version 3**. A replica from before this change accepts scope versions 1 and 2 and refuses anything else with
`503 coordinator unavailable`, so during a rolling upgrade an old replica that takes the job refuses it
(the job is released and another replica takes it) rather than running an embeddings call as chat and returning
a chat answer to a caller who asked for vectors. A chat job's scope is byte-identical to before (no new field,
version 1 or 2). Follow the transition rule of 0030 and `docs/src/proxy/scaling.md`: upgrade every replica before
relying on embeddings through the fleet.

The caller's own refusals are unchanged and run on the origin before the job exists: authentication, the key's
allowlist (and residency), guardrails, the batch and model checks, and the spend refusals (a capped key needs a
trusted input price, 0035). The worker's dispatch reads the operation from the trusted context and calls the
embeddings dispatch, so the usage record, spend observation and breaker are the same code as on one instance, and
the record names the tenant whichever replica dispatched.

## Not built

- Images, speech, transcription and streaming failover through a fleet.
- A refusal at the origin when no replica of the fleet understands version 3 (the job simply waits for one).

## Tests

`gateway::attempt::tests::an_embeddings_scope_is_version_3_with_its_operation_and_a_chat_scope_is_unchanged`
(unit): a chat scope has no `operation` field and keeps version 1, an embeddings scope is version 3 with
`operation` and round-trips, and a scope with no `operation` field reads as chat. The Redis-backed process tests
in `integration_serve_fleet` (two real replicas and a Redis): four embeddings requests, alternating replicas,
each answered with the provider's vectors and the provider hit exactly four times at `/embeddings` (not
`/chat/completions`), and images and speech still answer `501`. Not serializing the operation fails the first.
The rolling-upgrade refusal by an *older* binary is the older code's own check and is not exercised.
